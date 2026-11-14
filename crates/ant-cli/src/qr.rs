//! Minimal QR encoder for install-by-QR (roadmap P4, decision D-035 —
//! docs/adr/0004-install-by-qr-with-a-minimal-zero-dependency-qr-encoder.md).
//!
//! Scope: byte mode, error correction level M, versions 1-10 — exactly what
//! serving an install URL needs (a 60-byte URL is version 4). Larger modes,
//! other EC levels, and Kanji are out of scope on purpose; this module has
//! no dependencies and no unsafe code.
//!
//! Correctness: the format-information and version-information strings this
//! encoder computes are asserted against the published 32-string table
//! (Thonky QR tutorial, "Format and Version String Tables") in the tests
//! below, and the rendered matrices were additionally verified to decode
//! with the independent jsQR decoder (see `scripts/qr/verify-qr.js` and the
//! `qr_dump_matrices_for_external_check` test). The Reed-Solomon layout
//! follows the published block tables for versions 1-10 at level M.

use std::fmt::Write as _;

/// One rendered QR code: `modules[row][col]` is true for a dark module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrCode {
    size: usize,
    modules: Vec<Vec<bool>>,
}

/// Error correction level M block structure for versions 1-10:
/// (total data codewords, ec codewords per block, group-1 blocks,
/// group-1 data size, group-2 blocks, group-2 data size).
///
/// From the published "Error Correction Code Words and Block Information"
/// table; group 2 is empty for versions 1-7 at level M.
const BLOCKS_M: &[(usize, usize, usize, usize, usize, usize)] = &[
    (16, 10, 1, 16, 0, 0),
    (28, 16, 1, 28, 0, 0),
    (44, 26, 1, 44, 0, 0),
    (64, 18, 2, 32, 0, 0),
    (86, 24, 2, 43, 0, 0),
    (108, 16, 4, 27, 0, 0),
    (124, 18, 4, 31, 0, 0),
    (154, 22, 2, 38, 2, 39),
    (182, 22, 3, 36, 2, 37),
    (216, 26, 4, 43, 1, 44),
];

/// Alignment pattern center coordinates by version (1-10). Version 1 has
/// none. From the published alignment-pattern location table.
const ALIGNMENT: &[&[usize]] = &[
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
];

impl QrCode {
    /// Encode `text` (UTF-8 bytes) at level M, choosing the smallest
    /// version 1-10 that fits.
    pub fn encode(text: &str) -> Result<Self, String> {
        Self::encode_with_mask(text, None)
    }

    /// Encode with an optional forced mask (test seam: isolates mask
    /// selection from structure).
    fn encode_with_mask(text: &str, forced_mask: Option<usize>) -> Result<Self, String> {
        let payload = text.as_bytes();
        let (version_index, &(data_total, ec_len, g1_blocks, g1_size, g2_blocks, g2_size)) =
            BLOCKS_M
                .iter()
                .enumerate()
                .find(|&(index, &(total, _, _, _, _, _))| {
                    // Mode (4) + count (8, or 16 at version 10) + terminator
                    // (4) + payload must fit the version's data codewords.
                    let count_bits = if index >= 9 { 16 } else { 8 };
                    4 + count_bits + 4 + payload.len() * 8 <= total * 8
                })
                .ok_or_else(|| {
                    format!(
                        "QR: '{}' bytes need more than version 10 at level M; shorten the URL",
                        payload.len()
                    )
                })?;
        let version = version_index + 1;
        let size = 17 + 4 * version;

        // Data codewords: mode + count + payload + terminator + pad bytes.
        let count_bits = if version_index >= 9 { 16 } else { 8 };
        let mut bits: Vec<bool> = Vec::with_capacity(data_total * 8);
        push_bits(&mut bits, 0b0100, 4); // byte mode
        push_bits(&mut bits, payload.len() as u64, count_bits);
        for byte in payload {
            push_bits(&mut bits, *byte as u64, 8);
        }
        let terminator = 4.min(data_total * 8 - bits.len());
        push_bits(&mut bits, 0, terminator);
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut data: Vec<u8> = bits
            .chunks(8)
            .map(|chunk| chunk.iter().fold(0u8, |acc, bit| (acc << 1) | (*bit as u8)))
            .collect();
        for (i, pad) in [0xEC, 0x11].iter().cycle().enumerate() {
            if data.len() == data_total {
                break;
            }
            data.push(*pad);
            if i > data_total {
                return Err("QR: padding overflow".to_owned());
            }
        }
        if data.len() != data_total {
            return Err("QR: data codeword count mismatch".to_owned());
        }

        // Split into blocks, compute error correction per block.
        let block_count = g1_blocks + g2_blocks;
        let mut blocks: Vec<&[u8]> = Vec::with_capacity(block_count);
        let mut cursor = 0;
        for _ in 0..g1_blocks {
            blocks.push(&data[cursor..cursor + g1_size]);
            cursor += g1_size;
        }
        for _ in 0..g2_blocks {
            blocks.push(&data[cursor..cursor + g2_size]);
            cursor += g2_size;
        }
        if cursor != data_total {
            return Err("QR: block split mismatch".to_owned());
        }
        let ec: Vec<Vec<u8>> = blocks
            .iter()
            .map(|block| reed_solomon_remainder(block, ec_len))
            .collect();

        // Interleave: data codewords first, then error correction.
        let mut codewords: Vec<u8> = Vec::with_capacity(data_total + ec_len * block_count);
        let max_data = blocks.iter().map(|block| block.len()).max().unwrap_or(0);
        for i in 0..max_data {
            for block in &blocks {
                if i < block.len() {
                    codewords.push(block[i]);
                }
            }
        }
        for i in 0..ec_len {
            for block in &ec {
                codewords.push(block[i]);
            }
        }

        Self::draw(version, size, &codewords, forced_mask)
    }

    /// Build the matrix for a known codeword stream: function patterns,
    /// codeword placement, then the best-scoring mask (or the forced one).
    fn draw(
        version: usize,
        size: usize,
        codewords: &[u8],
        forced_mask: Option<usize>,
    ) -> Result<Self, String> {
        let mut modules = vec![vec![false; size]; size];
        let mut functions = vec![vec![false; size]; size];

        // Finder patterns with separators (includes the light ring).
        for &(cx, cy) in &[(3, 3), (size - 4, 3), (3, size - 4)] {
            let cx = cx as isize;
            let cy = cy as isize;
            for dy in -4..=4isize {
                for dx in -4..=4isize {
                    let x = cx + dx;
                    let y = cy + dy;
                    if x < 0 || y < 0 || x >= size as isize || y >= size as isize {
                        continue;
                    }
                    let dist = dx.abs().max(dy.abs());
                    set_fn(
                        &mut modules,
                        &mut functions,
                        x as usize,
                        y as usize,
                        dist != 2 && dist != 4,
                    );
                }
            }
        }

        // Timing patterns.
        for i in 8..size - 8 {
            let dark = i % 2 == 0;
            if !functions[6][i] {
                set_fn(&mut modules, &mut functions, i, 6, dark);
            }
            if !functions[i][6] {
                set_fn(&mut modules, &mut functions, 6, i, dark);
            }
        }

        // Alignment patterns (skipping finder overlaps).
        let positions = ALIGNMENT[version - 1];
        for &cy in positions {
            for &cx in positions {
                if cx == 6 && (cy == 6 || cy == size - 7) || cy == 6 && cx == size - 7 {
                    continue;
                }
                for dy in -2..=2isize {
                    for dx in -2..=2isize {
                        let dist = dx.abs().max(dy.abs());
                        set_fn(
                            &mut modules,
                            &mut functions,
                            (cx as isize + dx) as usize,
                            (cy as isize + dy) as usize,
                            dist != 1,
                        );
                    }
                }
            }
        }

        // Reserve format information areas (drawn after masking is chosen).
        for i in 0..9 {
            if i != 6 {
                let (x, y) = (8usize, i); // column 8, rows 0-5 and 7-8
                functions[y][x] = true;
                modules[y][x] = false;
                let (x, y) = (i, 8usize); // row 8, columns 0-5 and 7-8
                functions[y][x] = true;
                modules[y][x] = false;
            }
        }
        for i in 0..8 {
            let (x, y) = (size - 1 - i, 8usize);
            functions[y][x] = true;
            modules[y][x] = false;
            let (x, y) = (8usize, size - 1 - i);
            functions[y][x] = true;
            modules[y][x] = false;
        }
        let (x, y) = (8usize, size - 8);
        functions[y][x] = true;
        modules[y][x] = true; // the dark module

        // Version information (versions 7+).
        if version >= 7 {
            let bits = version_information_bits(version);
            for i in 0..18 {
                let bit = (bits >> i) & 1 == 1;
                let a = size - 11 + i % 3;
                let b = i / 3;
                set_fn(&mut modules, &mut functions, a, b, bit);
                set_fn(&mut modules, &mut functions, b, a, bit);
            }
        }

        // Place the codeword stream in the zig-zag order. Trailing remainder
        // bits (versions 2-6) are zero, so they simply stay light: the loop
        // stops when the codeword bits run out.
        let mut bit_index = 0usize;
        // Column pairs proceed right-to-left in steps of two; the timing
        // column (6) is skipped, which shifts the rest of the sequence by
        // one (6 -> 5, then 3, 1). The loop ends at the pair (1, 0).
        let mut right = size as isize - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            let right_col = right as usize;
            for vertical in 0..size {
                let upward = ((right_col + 1) & 2) == 0;
                for j in 0..2 {
                    let x = right_col - j;
                    let y = if upward {
                        size - 1 - vertical
                    } else {
                        vertical
                    };
                    if !functions[y][x] && bit_index < codewords.len() * 8 {
                        let byte = codewords[bit_index / 8];
                        let bit = (byte >> (7 - bit_index % 8)) & 1 == 1;
                        modules[y][x] = bit;
                        bit_index += 1;
                    }
                }
            }
            right -= 2;
        }

        // Choose the mask with the lowest penalty score (or the forced one),
        // then apply it and write the format information.
        let chosen_mask = forced_mask.unwrap_or_else(|| {
            let mut best_mask = 0usize;
            let mut best_score = usize::MAX;
            for mask in 0..8 {
                let mut candidate = modules.clone();
                apply_mask(&mut candidate, &functions, mask);
                let score = penalty_score(&candidate);
                if score < best_score {
                    best_score = score;
                    best_mask = mask;
                }
            }
            best_mask
        });
        let mut result = modules.clone();
        apply_mask(&mut result, &functions, chosen_mask);
        let format = format_information_bits(chosen_mask as u8);
        apply_format(&mut result, &functions, format, size);
        Ok(Self {
            size,
            modules: result,
        })
    }

    #[cfg(test)]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Module state by (row, column): true is dark.
    #[cfg(test)]
    pub fn module(&self, row: usize, column: usize) -> bool {
        self.modules[row][column]
    }

    /// Render with a 4-module quiet zone using half-block characters, two
    /// matrix rows per terminal line. Light background assumed.
    pub fn render_terminal(&self) -> String {
        let quiet = 4;
        let total = self.size + 2 * quiet;
        let light = |row: usize, col: usize| {
            row >= quiet
                && col >= quiet
                && row < quiet + self.size
                && col < quiet + self.size
                && self.modules[row - quiet][col - quiet]
        };
        let mut out = String::new();
        let rows: Vec<String> = (0..total)
            .step_by(2)
            .map(|top| {
                (0..total)
                    .map(|col| match (light(top, col), light(top + 1, col)) {
                        (true, true) => "█",
                        (true, false) => "▀",
                        (false, true) => "▄",
                        (false, false) => " ",
                    })
                    .collect()
            })
            .collect();
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
        out
    }
}

impl std::fmt::Display for QrCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render_terminal())
    }
}

/// Mark a module as a function module and set its color.
fn set_fn(modules: &mut [Vec<bool>], functions: &mut [Vec<bool>], x: usize, y: usize, dark: bool) {
    modules[y][x] = dark;
    functions[y][x] = true;
}

/// Write the 15-bit format information for level M and `mask`, masked with
/// the spec pattern 0x5412 (BCH over `00 + mask`).
fn format_information_bits(mask: u8) -> u16 {
    let data: u16 = (mask & 0b111) as u16; // EC level M bits are 00.
    let mut remainder = data;
    for _ in 0..10 {
        remainder = (remainder << 1) ^ ((remainder >> 9) * 0x537);
    }
    ((data << 10) | remainder) ^ 0x5412
}

/// 18-bit version information (BCH over the version number).
fn version_information_bits(version: usize) -> u32 {
    let mut remainder = version as u32;
    for _ in 0..12 {
        remainder = (remainder << 1) ^ ((remainder >> 11) * 0x1F25);
    }
    (version as u32) << 12 | remainder
}

/// Mask `modules` with pattern `mask` outside function areas.
fn apply_mask(modules: &mut [Vec<bool>], functions: &[Vec<bool>], mask: usize) {
    for (y, row) in modules.iter_mut().enumerate() {
        for (x, dark) in row.iter_mut().enumerate() {
            if functions[y][x] {
                continue;
            }
            let invert = match mask {
                0 => (x + y) % 2 == 0,
                1 => y % 2 == 0,
                2 => x % 3 == 0,
                3 => (x + y) % 3 == 0,
                4 => (x / 3 + y / 2) % 2 == 0,
                5 => x * y % 2 + x * y % 3 == 0,
                6 => (x * y % 2 + x * y % 3) % 2 == 0,
                7 => ((x + y) % 2 + x * y % 3) % 2 == 0,
                _ => false,
            };
            if invert {
                *dark = !*dark;
            }
        }
    }
}

/// Overwrite format-information modules (never masked) with the final bits.
fn apply_format(modules: &mut [Vec<bool>], _functions: &[Vec<bool>], format: u16, size: usize) {
    for i in 0..15 {
        let bit = (format >> i) & 1 == 1;
        match i {
            0..=5 => modules[i][8] = bit,
            6 => modules[7][8] = bit,
            7 => modules[8][8] = bit,
            8 => modules[8][7] = bit,
            9..=14 => modules[8][14 - i] = bit,
            _ => unreachable!(),
        }
        if i < 8 {
            modules[8][size - 1 - i] = bit;
        } else {
            modules[size - 15 + i][8] = bit;
        }
    }
}

/// Penalty score of a masked matrix (lower is better): runs, blocks,
/// finder-like patterns, and dark proportion, per the spec.
fn line_score(mut line: impl Iterator<Item = bool>) -> usize {
    let Some(mut run_color) = line.next() else {
        return 0;
    };
    let mut run_score = 0usize;
    let mut run_length = 1usize;
    for value in line {
        if value == run_color {
            run_length += 1;
        } else {
            if run_length >= 5 {
                run_score += run_length - 2;
            }
            run_color = value;
            run_length = 1;
        }
    }
    if run_length >= 5 {
        run_score += run_length - 2;
    }
    run_score
}

fn penalty_score(modules: &[Vec<bool>]) -> usize {
    let size = modules.len();
    let mut score = 0usize;

    // Rule 1: five or more same-colored modules in a row/column.
    for row in modules {
        score += line_score(row.iter().copied());
    }
    for x in 0..size {
        score += line_score(modules.iter().map(|row| row[x]));
    }

    // Rule 2: 2x2 blocks of the same color.
    for y in 0..size - 1 {
        for x in 0..size - 1 {
            let color = modules[y][x];
            if modules[y][x + 1] == color
                && modules[y + 1][x] == color
                && modules[y + 1][x + 1] == color
            {
                score += 3;
            }
        }
    }

    // Rule 3: finder-like patterns (1:1:3:1:1 ratio with four light
    // modules before or after), in rows and columns.
    let pattern = [true, false, true, true, true, false, true];
    for y in 0..size {
        for x in 0..size {
            if x + 7 <= size {
                let mut matches = true;
                for (i, dark) in pattern.iter().enumerate() {
                    if modules[y][x + i] != *dark {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    let light_before = x >= 4 && (x - 4..x).all(|i| !modules[y][i]);
                    let light_after = x + 11 <= size && (x + 7..x + 11).all(|i| !modules[y][i]);
                    if light_before || light_after {
                        score += 40;
                    }
                }
            }
            if y + 7 <= size {
                let mut matches = true;
                for (i, dark) in pattern.iter().enumerate() {
                    if modules[y + i][x] != *dark {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    let light_before = y >= 4 && (y - 4..y).all(|i| !modules[i][x]);
                    let light_after = y + 11 <= size && (y + 7..y + 11).all(|i| !modules[i][x]);
                    if light_before || light_after {
                        score += 40;
                    }
                }
            }
        }
    }

    // Rule 4: proportion of dark modules (each 5% step from 50% costs 10).
    let dark = modules
        .iter()
        .flat_map(|row| row.iter().filter(|bit| **bit))
        .count();
    let total = size * size;
    let percent = dark * 100 / total;
    let previous = percent / 5 * 5;
    let next = previous + 5;
    let deviation = (previous as isize - 50)
        .abs()
        .min((next as isize - 50).abs()) as usize;
    score += deviation / 5 * 10;
    score
}

fn push_bits(bits: &mut Vec<bool>, value: u64, count: usize) {
    for i in (0..count).rev() {
        bits.push((value >> i) & 1 == 1);
    }
}

/// Reed-Solomon remainder over GF(256) with the QR primitive polynomial
/// 0x11D: returns `ec_len` codewords for `data`.
fn reed_solomon_remainder(data: &[u8], ec_len: usize) -> Vec<u8> {
    // Generator polynomial coefficients: product of (x - alpha^i), i in
    // 0..ec_len, highest degree first, leading coefficient omitted below.
    let mut generator: Vec<u8> = vec![1];
    for i in 0..ec_len {
        let mut next = vec![0u8; generator.len() + 1];
        for (j, coefficient) in generator.iter().enumerate() {
            next[j] ^= gf_mul(*coefficient, 1);
            next[j + 1] ^= gf_mul(*coefficient, gf_pow(i));
        }
        generator = next;
    }
    // Synthetic division of data || 0*ec_len by the generator.
    let mut result = data.to_vec();
    result.resize(data.len() + ec_len, 0);
    for i in 0..data.len() {
        let factor = result[i];
        if factor == 0 {
            continue;
        }
        for (j, coefficient) in generator.iter().enumerate().skip(1) {
            result[i + j] ^= gf_mul(*coefficient, factor);
        }
    }
    result[data.len()..].to_vec()
}

/// Multiplication in GF(256), QR primitive polynomial 0x11D.
fn gf_mul(a: u8, b: u8) -> u8 {
    let mut product = 0u16;
    let mut a = a as u16;
    let mut b = b;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        b >>= 1;
        a <<= 1;
        if a & 0x100 != 0 {
            a ^= 0x11D;
        }
    }
    product as u8
}

/// 2^exponent in GF(256).
fn gf_pow(exponent: usize) -> u8 {
    let mut value = 1u8;
    for _ in 0..exponent {
        value = gf_mul(value, 2);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published 15-bit format strings for level M, masks 0-7
    /// (Thonky "Format and Version String Tables" table).
    const FORMAT_M: [&str; 8] = [
        "101010000010010",
        "101000100100101",
        "101111001111100",
        "101101101001011",
        "100010111111001",
        "100000011001110",
        "100111110010111",
        "100101010100000",
    ];

    /// The published 18-bit version information strings, versions 7-10.
    const VERSION_INFO: [(&str, &str); 4] = [
        ("7", "000111110010010100"),
        ("8", "001000010110111100"),
        ("9", "001001101010011001"),
        ("10", "001010010011010011"),
    ];

    #[test]
    fn qr_format_strings_match_published_table() {
        for (mask, expected) in FORMAT_M.iter().enumerate() {
            let bits = format_information_bits(mask as u8);
            let rendered = format!("{bits:015b}");
            assert_eq!(&rendered, expected, "mask {mask}");
        }
    }

    #[test]
    fn qr_version_strings_match_published_table() {
        for (version, expected) in VERSION_INFO {
            let bits = version_information_bits(version.parse().unwrap());
            assert_eq!(format!("{bits:018b}"), expected, "version {version}");
        }
    }

    #[test]
    fn qr_generator_polynomial_matches_tutorial() {
        // (x - a^0)(x - a^1) = x^2 + 3x + 2 in GF(256): Thonky's worked
        // "generator polynomial for 2 error correction codewords".
        let mut generator = vec![1u8];
        for i in 0..2 {
            let mut next = vec![0u8; generator.len() + 1];
            for (j, coefficient) in generator.iter().enumerate() {
                next[j] ^= gf_mul(*coefficient, 1);
                next[j + 1] ^= gf_mul(*coefficient, gf_pow(i));
            }
            generator = next;
        }
        assert_eq!(generator, vec![1, 3, 2]);
    }

    #[test]
    fn qr_reed_solomon_remainder_divides_cleanly() {
        let data: Vec<u8> = (0..16u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(11))
            .collect();
        let ec = reed_solomon_remainder(&data, 10);
        assert_eq!(ec.len(), 10);
        let mut message = data.clone();
        message.extend_from_slice(&ec);
        let mut generator: Vec<u8> = vec![1];
        for i in 0..10 {
            let mut next = vec![0u8; generator.len() + 1];
            for (j, coefficient) in generator.iter().enumerate() {
                next[j] ^= gf_mul(*coefficient, 1);
                next[j + 1] ^= gf_mul(*coefficient, gf_pow(i));
            }
            generator = next;
        }
        let mut result = message.clone();
        result.resize(message.len() + 10, 0);
        for i in 0..message.len() {
            let factor = result[i];
            if factor == 0 {
                continue;
            }
            for (j, coefficient) in generator.iter().enumerate().skip(1) {
                result[i + j] ^= gf_mul(*coefficient, factor);
            }
        }
        assert!(result[message.len()..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn qr_capacity_is_exact() {
        // 14 bytes is the exact version 1-M byte-mode capacity.
        assert_eq!(QrCode::encode(&"x".repeat(14)).unwrap().size(), 21);
        // 15 bytes selects the next version up, not an error.
        assert_eq!(QrCode::encode(&"x".repeat(15)).unwrap().size(), 25);
        assert_eq!(QrCode::encode(&"x".repeat(26)).unwrap().size(), 25);
        // Version 10-M holds exactly 213 byte-mode bytes.
        assert_eq!(QrCode::encode(&"x".repeat(213)).unwrap().size(), 57);
        assert!(QrCode::encode(&"x".repeat(214)).is_err());
        // A typical install URL selects version 3.
        assert_eq!(
            QrCode::encode("http://192.168.1.100:39721/app-release.apk")
                .unwrap()
                .size(),
            29
        );
    }

    #[test]
    fn qr_structure_has_finder_timing_and_dark_modules() {
        let code = QrCode::encode("ant").unwrap();
        let size = code.size();
        // Finder centers.
        assert!(code.module(3, 3) && code.module(3, size - 4) && code.module(size - 4, 3));
        // Separators stay light.
        assert!(!code.module(3, 7) && !code.module(7, 3));
        // Timing pattern alternates.
        for i in 8..size - 8 {
            assert_eq!(code.module(6, i), i % 2 == 0, "timing at {i}");
            assert_eq!(code.module(i, 6), i % 2 == 0, "timing at {i}");
        }
        // The dark module.
        assert!(code.module(size - 8, 8));
    }

    #[test]
    fn qr_alignment_patterns_are_placed() {
        let code = QrCode::encode(&"y".repeat(30)).unwrap(); // version 3
        assert_eq!(ALIGNMENT[2], [6, 22]);
        // The (22, 22) alignment pattern: dark center and dark outer ring
        // (distances 0 and 2), light ring at distance 1.
        assert!(code.module(22, 22));
        assert!(code.module(20, 22) && code.module(24, 22));
        assert!(code.module(22, 20) && code.module(22, 24));
        assert!(code.module(20, 20) && code.module(20, 24));
        assert!(code.module(24, 20) && code.module(24, 24));
        assert!(!code.module(21, 22));
        assert!(!code.module(22, 21));
        assert!(!code.module(23, 22));
        assert!(!code.module(22, 23));
    }

    #[test]
    fn qr_encoding_is_deterministic() {
        let a = QrCode::encode("http://10.0.0.2:8080/app.apk").unwrap();
        let b = QrCode::encode("http://10.0.0.2:8080/app.apk").unwrap();
        assert_eq!(a, b);
        let c = QrCode::encode("http://10.0.0.2:8081/app.apk").unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn qr_render_uses_half_blocks_and_quiet_zone() {
        let code = QrCode::encode("ant").unwrap();
        let rendered = code.render_terminal();
        let width = rendered.lines().next().unwrap().chars().count();
        // (21 + 2*4 quiet) columns, one char per two matrix rows.
        assert_eq!(width, 29);
        assert_eq!(rendered.lines().count(), 15);
        assert!(rendered.contains('█') || rendered.contains('▀'));
    }

    /// Writes plain-text matrices (one '#'/'.' per module, no quiet zone)
    /// for a spread of payloads into the system temp dir, where
    /// `scripts/qr/verify-qr.js` decodes each with the independent jsQR
    /// decoder. Run that script after `cargo test -p ant-cli qr` to confirm
    /// real scanners read these matrices.
    #[test]
    fn qr_dump_matrices_for_external_check() {
        let dir = std::env::temp_dir().join("ant-qr-check");
        std::fs::create_dir_all(&dir).expect("create dir");
        for (i, payload) in [
            "ant",
            "http://192.168.1.100:39721/app-release.apk",
            "https://ant.example.internal:8443/dist/app-release-signed.apk",
        ]
        .iter()
        .enumerate()
        {
            let code = QrCode::encode(payload).expect("encode");
            let mut out = format!("# {payload}\n{}\n", code.size());
            for row in 0..code.size() {
                for col in 0..code.size() {
                    out.push(if code.module(row, col) { '#' } else { '.' });
                }
                out.push('\n');
            }
            std::fs::write(dir.join(format!("matrix-{i}.txt")), out).expect("write matrix");
        }
        // Forced-mask variants of the first payload: the external decoder
        // script checks every mask so a mask-selection bug cannot hide.
        for mask in 0..8 {
            let code = QrCode::encode_with_mask("ant", Some(mask)).expect("encode");
            let mut out = format!("# ant\n{}\n", code.size());
            for row in 0..code.size() {
                for col in 0..code.size() {
                    out.push(if code.module(row, col) { '#' } else { '.' });
                }
                out.push('\n');
            }
            std::fs::write(dir.join(format!("mask-m{mask}.txt")), out).expect("write matrix");
        }
    }
}
