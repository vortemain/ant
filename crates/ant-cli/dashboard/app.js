// Ant local dashboard (D-042). All data comes from this origin's api/builds
// and is rendered through DOM APIs with textContent (never innerHTML).
"use strict";

const REFRESH_MS = 3000;
const DAY = 86400000;
const SVG = "http://www.w3.org/2000/svg";

const state = {
  builds: [],
  running: [],
  clockOffset: 0, // server clock minus browser clock
  now: Date.now(),
  page: "overview",
  filter: "all",
  query: "",
  project: null, // project path the builds page is narrowed to
  expanded: new Set(),
  lastOk: 0,
  loaded: false,
  signature: "",
  renderedAt: 0,
};

const $ = (id) => document.getElementById(id);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function svgEl(tag, attrs) {
  const node = document.createElementNS(SVG, tag);
  for (const [key, value] of Object.entries(attrs || {})) node.setAttribute(key, String(value));
  return node;
}

function icon(path) {
  const svg = svgEl("svg", { viewBox: "0 0 24 24", "aria-hidden": "true" });
  svg.append(svgEl("path", { d: path }));
  return svg;
}

// ---------- formatting ----------

function projectName(path) {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

function duration(ms) {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${Math.round(s % 60)} s`;
  return `${Math.floor(m / 60)} h ${m % 60} min`;
}

function ago(ms, now) {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h ago`;
  const d = Math.round(h / 24);
  if (d < 14) return `${d} d ago`;
  return new Date(ms).toLocaleDateString();
}

function bytes(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function percent(part, whole) {
  return whole ? `${Math.round((part / whole) * 100)}%` : "–";
}

const TOOL_LABELS = {
  cargo: ["Rs", "Rust · cargo"], go: ["Go", "Go"], npm: ["JS", "npm"], cmake: ["C", "CMake"],
  gradle: ["An", "Android · Gradle"], flutter: ["Fl", "Flutter"], xcode: ["iOS", "Xcode"],
  maven: ["Mv", "Maven"], python: ["Py", "Python"], tauri: ["Ta", "Tauri"],
  electron: ["El", "Electron"], docker: ["Dk", "Docker"], dotnet: ["NET", ".NET"],
};

// Official marks from Simple Icons 13.21.0 (CC0; the logos are their owners' trademarks),
// embedded as path data so the dashboard loads nothing from outside (D-061).
const LOGOS = {
  "android": "M18.4395 5.5586c-.675 1.1664-1.352 2.3318-2.0274 3.498-.0366-.0155-.0742-.0286-.1113-.043-1.8249-.6957-3.484-.8-4.42-.787-1.8551.0185-3.3544.4643-4.2597.8203-.084-.1494-1.7526-3.021-2.0215-3.4864a1.1451 1.1451 0 0 0-.1406-.1914c-.3312-.364-.9054-.4859-1.379-.203-.475.282-.7136.9361-.3886 1.5019 1.9466 3.3696-.0966-.2158 1.9473 3.3593.0172.031-.4946.2642-1.3926 1.0177C2.8987 12.176.452 14.772 0 18.9902h24c-.119-1.1108-.3686-2.099-.7461-3.0683-.7438-1.9118-1.8435-3.2928-2.7402-4.1836a12.1048 12.1048 0 0 0-2.1309-1.6875c.6594-1.122 1.312-2.2559 1.9649-3.3848.2077-.3615.1886-.7956-.0079-1.1191a1.1001 1.1001 0 0 0-.8515-.5332c-.5225-.0536-.9392.3128-1.0488.5449zm-.0391 8.461c.3944.5926.324 1.3306-.1563 1.6503-.4799.3197-1.188.0985-1.582-.4941-.3944-.5927-.324-1.3307.1563-1.6504.4727-.315 1.1812-.1086 1.582.4941zM7.207 13.5273c.4803.3197.5506 1.0577.1563 1.6504-.394.5926-1.1038.8138-1.584.4941-.48-.3197-.5503-1.0577-.1563-1.6504.4008-.6021 1.1087-.8106 1.584-.4941z", // Android
  "apachemaven": "M4.237.001c-.312-.013-.665.072-.828.457-.158.374-.283 1.188-.34 2.276l1.223.591c-.02-.737.007-1.43.076-2.066-.026.299-.056.96.006 2.039.019.342.049.725.088 1.15.002.024.002.047.007.069a45.485 45.485 0 0 0 .309 2.412c.057.368.126.752.195 1.16l-.01.01c.014.01.015.018.014.023l.03.16c.03.162.06.328.093.494l.108.553.056.289a61.72 61.72 0 0 0 .457 2.068c.09.382.186.78.287 1.186.098.386.199.783.309 1.193.096.362.199.735.303 1.117.003.018.012.036.015.055a145.826 145.826 0 0 0 .34 1.185l.049.174c.078.261.158.533.242.805a4.2 4.2 0 0 1-.293-.135l-.19-.654c-.02-.077-.042-.148-.062-.225l-.002-.004-.004-.002c-.087-.3-.17-.607-.257-.916-.023-.087-.044-.173-.069-.263l-.314-1.178c-.1-.381-.194-.765-.29-1.154-.094-.39-.185-.78-.277-1.172-.093-.401-.181-.8-.265-1.203-.085-.396-.161-.798-.24-1.193a50.315 50.315 0 0 1-.211-1.17c-.004-.013-.006-.03-.01-.041l.004-.002c-.057-.386-.116-.77-.174-1.15a60.905 60.905 0 0 1-.154-1.204 27.447 27.447 0 0 1-.172-2.41l-1.22-.59c-.004.074-.01.15-.013.23-.012.294-.02.605-.023.93a45.3 45.3 0 0 0 .006 1.157c.009.37.025.755.045 1.148.02.336.042.675.07 1.022l.002.039.006.004c.003.023.007.05.006.076.033.368.064.739.107 1.115a34.493 34.493 0 0 0 .303 2.125c.01.064.024.131.035.195a23.418 23.418 0 0 0 .547 2.32c.07.237.14.464.21.68.063.182.13.365.194.545.155.422.327.832.512 1.232l.006.004a.318.318 0 0 0 .02.05c.225.485.475.95.755 1.395.01.013.02.033.03.047-.455-.183-1.259-.098-1.253-.097.83.288 1.557.64 2.016 1.175-.183.2-.523.352-.953.477.594.064.924-.039 1.045-.092-.31.26-.483.732-.635 1.24.35-.57.696-.949 1.033-1.094.078.258.162.524.244.788A147.532 147.532 0 0 0 5.157 24a.56.56 0 0 0 .43-.312c.13-.282.83-1.775 1.908-3.875.413 1.303.88 2.679 1.386 4.109a.494.494 0 0 0 .076-.465 103.735 103.735 0 0 1-1.308-3.945c.154-.299.316-.612.484-.932.125.04.255.094.389.155.203.186.352.491.482.84a1.515 1.515 0 0 0-.334-1.098c1.335.258 2.547.09 3.287-.81a3.97 3.97 0 0 0 .192-.258c-.325.304-.682.404-1.313.273.996-.281 1.523-.617 2.035-1.22.12-.145.244-.303.371-.48-.943.722-1.927.822-2.9.493l-.045-.018c.914.02 2.203-.474 3.092-1.189.41-.33.796-.73 1.17-1.21.28-.359.55-.76.82-1.216.234-.393.468-.824.7-1.293a2.83 2.83 0 0 1-.74.137l-.144.008c-.048.002-.093 0-.146.002.885-.198 1.5-.74 1.994-1.447-.24.117-.628.262-1.07.297-.058.006-.12.006-.182.006-.013-.002-.028 0-.047-.002.306-.078.574-.178.81-.309a3.363 3.363 0 0 0 .358-.236c.044-.037.088-.07.13-.106.099-.086.193-.18.28-.287.028-.034.056-.063.08-.098.036-.05.073-.098.104-.146a8.388 8.388 0 0 0 .51-.828c.015-.031.032-.057.046-.088.04-.084.08-.16.11-.227.042-.099.074-.179.092-.238a.515.515 0 0 1-.108.051c-.273.112-.727.187-1.086.201-.004 0-.008 0-.013.004h-.067c.72-.214 1.067-.45 1.422-.818a13.883 13.883 0 0 0 1.154-1.428c.264-.37.505-.738.692-1.072a6.5 6.5 0 0 0 .298-.592c.066-.157.122-.305.172-.45-.466.01-.986.011-1.48 0 .495.01 1.015.007 1.484-.005.5-1.485.063-2.262.063-2.262s-.526-1.212-1.4-.851c-.426.175-1.172.73-2.083 1.56l.514 1.45a17.561 17.561 0 0 1 1.703-1.602c-.257.22-.807.726-1.615 1.644-.256.29-.537.624-.844.997-.017.02-.035.038-.047.06a51.435 51.435 0 0 0-1.666 2.187c-.248.34-.498.704-.765 1.088h-.016c.002.02-.004.028-.01.032l-.101.152c-.104.155-.213.31-.318.47l-.352.534c-.061.09-.124.181-.186.277-.184.282-.367.573-.558.873a97.351 97.351 0 0 0-1.428 2.338 96.866 96.866 0 0 0-1.341 2.343c-.012.017-.02.04-.034.057a197.256 197.256 0 0 0-.668 1.223l-.097.181c-.17.318-.346.642-.52.979 0 .004-.005.008-.006.013-.026.048-.05.093-.072.141-.117.222-.218.424-.45.87a1.352 1.352 0 0 0-.233-.182l.345-.65c.047-.089.096-.177.143-.27l.04-.077.546-1.001.13-.233v-.006l-.001-.006c.169-.31.345-.62.52-.94.051-.087.102-.173.153-.265.224-.395.454-.794.684-1.197a91.685 91.685 0 0 1 2.135-3.504c.247-.386.503-.77.754-1.152.092-.138.182-.272.279-.41a72.9 72.9 0 0 1 .48-.701c.007-.012.019-.024.026-.037h.006c.26-.356.517-.713.773-1.065.278-.373.554-.735.83-1.09a31.075 31.075 0 0 1 1.777-2.075l-.515-1.446c-.06.057-.126.116-.192.178a32.37 32.37 0 0 0-.758.729c-.295.294-.597.606-.912.935a46.032 46.032 0 0 0-1.632 1.838l-.03.033.002.008c-.017.02-.033.044-.054.064-.266.323-.538.649-.801.985a39.105 39.105 0 0 0-1.445 1.95c-.043.06-.085.126-.127.186a26.458 26.458 0 0 0-1.403 2.303c-.13.247-.256.485-.37.715-.096.195-.187.395-.278.591-.21.463-.398.93-.566 1.399l.002.006a.36.36 0 0 0-.026.058c-.108.303-.203.608-.29.914-.14.174-.302.325-.483.46a3.505 3.505 0 0 0-.131-.153 5.148 5.148 0 0 0 .824-2.211 6.4 6.4 0 0 0-.016-1.488c-.046-.4-.126-.82-.238-1.274-.097-.393-.217-.81-.363-1.248-.091.185-.22.367-.379.545l-.086.094c-.029.032-.06.06-.092.094.434-.674.486-1.397.358-2.148a2.722 2.722 0 0 1-.49.85c-.033.038-.072.077-.11.116-.01.007-.019.018-.033.028.144-.24.25-.467.318-.698a1.29 1.29 0 0 0 .04-.146 2.85 2.85 0 0 0 .038-.225l.018-.146a2.11 2.11 0 0 0-.002-.354c-.003-.04-.004-.076-.01-.113-.01-.055-.016-.105-.027-.154a7.416 7.416 0 0 0-.193-.84c-.01-.028-.015-.056-.026-.084-.027-.079-.048-.149-.072-.209a2.1 2.1 0 0 0-.09-.209.455.455 0 0 1-.035.1c-.102.24-.34.57-.557.8-.003.003-.007.005-.007.01l-.04.043c.318-.58.39-.946.385-1.398a12.274 12.274 0 0 0-.16-1.615 10.68 10.68 0 0 0-.232-1.104 5.853 5.853 0 0 0-.18-.558 6.337 6.337 0 0 0-.172-.391 26.18 26.18 0 0 0 .002-.004C5.576.341 4.82.124 4.82.124s-.27-.11-.582-.123zm3.38 15.783l.032.082v.002c-.06.033-.116.067-.178.097-.012.004-.024.012-.039.018a2.41 2.41 0 0 0 .186-.2zm-.603 1.626c.13.136.25.242.354.32l.07.227a1.866 1.866 0 0 0-.246.053l-.03-.098c-.024-.084-.048-.17-.076-.257l-.021-.073zm.26.875a2.34 2.34 0 0 1 .271.01l.07.229a.778.778 0 0 1 .247-.004l-.326.627a127.643 127.643 0 0 1-.262-.862z", // Apache Maven
  "cmake": "M11.769.066L.067 23.206l12.76-10.843zM23.207 23.934L7.471 17.587 0 23.934zM24 23.736L12.298.463l1.719 19.24zM12.893 12.959l-5.025 4.298 5.62 2.248z", // CMake
  "docker": "M13.983 11.078h2.119a.186.186 0 00.186-.185V9.006a.186.186 0 00-.186-.186h-2.119a.185.185 0 00-.185.185v1.888c0 .102.083.185.185.185m-2.954-5.43h2.118a.186.186 0 00.186-.186V3.574a.186.186 0 00-.186-.185h-2.118a.185.185 0 00-.185.185v1.888c0 .102.082.185.185.185m0 2.716h2.118a.187.187 0 00.186-.186V6.29a.186.186 0 00-.186-.185h-2.118a.185.185 0 00-.185.185v1.887c0 .102.082.185.185.186m-2.93 0h2.12a.186.186 0 00.184-.186V6.29a.185.185 0 00-.185-.185H8.1a.185.185 0 00-.185.185v1.887c0 .102.083.185.185.186m-2.964 0h2.119a.186.186 0 00.185-.186V6.29a.185.185 0 00-.185-.185H5.136a.186.186 0 00-.186.185v1.887c0 .102.084.185.186.186m5.893 2.715h2.118a.186.186 0 00.186-.185V9.006a.186.186 0 00-.186-.186h-2.118a.185.185 0 00-.185.185v1.888c0 .102.082.185.185.185m-2.93 0h2.12a.185.185 0 00.184-.185V9.006a.185.185 0 00-.184-.186h-2.12a.185.185 0 00-.184.185v1.888c0 .102.083.185.185.185m-2.964 0h2.119a.185.185 0 00.185-.185V9.006a.185.185 0 00-.184-.186h-2.12a.186.186 0 00-.186.186v1.887c0 .102.084.185.186.185m-2.92 0h2.12a.185.185 0 00.184-.185V9.006a.185.185 0 00-.184-.186h-2.12a.185.185 0 00-.184.185v1.888c0 .102.082.185.185.185M23.763 9.89c-.065-.051-.672-.51-1.954-.51-.338.001-.676.03-1.01.087-.248-1.7-1.653-2.53-1.716-2.566l-.344-.199-.226.327c-.284.438-.49.922-.612 1.43-.23.97-.09 1.882.403 2.661-.595.332-1.55.413-1.744.42H.751a.751.751 0 00-.75.748 11.376 11.376 0 00.692 4.062c.545 1.428 1.355 2.48 2.41 3.124 1.18.723 3.1 1.137 5.275 1.137.983.003 1.963-.086 2.93-.266a12.248 12.248 0 003.823-1.389c.98-.567 1.86-1.288 2.61-2.136 1.252-1.418 1.998-2.997 2.553-4.4h.221c1.372 0 2.215-.549 2.68-1.009.309-.293.55-.65.707-1.046l.098-.288Z", // Docker
  "dotnet": "M24 8.77h-2.468v7.565h-1.425V8.77h-2.462V7.53H24zm-6.852 7.565h-4.821V7.53h4.63v1.24h-3.205v2.494h2.953v1.234h-2.953v2.604h3.396zm-6.708 0H8.882L4.78 9.863a2.896 2.896 0 0 1-.258-.51h-.036c.032.189.048.592.048 1.21v5.772H3.157V7.53h1.659l3.965 6.32c.167.261.275.442.323.54h.024c-.04-.233-.06-.629-.06-1.185V7.529h1.372zm-8.703-.693a.868.829 0 0 1-.869.829.868.829 0 0 1-.868-.83.868.829 0 0 1 .868-.828.868.829 0 0 1 .869.829Z", // .NET
  "electron": "M12.0111 0c-.85 0-1.5392.6891-1.5392 1.5392 0 .8501.6891 1.5393 1.5392 1.5393.595 0 1.11-.338 1.3662-.832 2.2208 1.2675 3.847 5.4728 3.847 10.3623 0 2.0715-.2891 4.056-.825 5.7685a.3215.3215 0 0 0 .2107.403.322.322 0 0 0 .4033-.2111c.5558-1.7763.8542-3.8251.8542-5.9604 0-5.1927-1.7717-9.686-4.3206-11.0027.001-.0223.0035-.0443.0035-.0669 0-.85-.6891-1.5392-1.5393-1.5392zm0 .6432a.896.896 0 1 1 0 1.792.896.896 0 1 1 0-1.792zm-5.486 4.3052c-2.067.0074-3.6473.6646-4.3885 1.9485-.7375 1.2774-.5267 2.971.5113 4.7813a.3217.3217 0 0 0 .558-.32C2.271 9.7274 2.089 8.266 2.6938 7.2185c.821-1.422 3.033-1.9552 5.9321-1.4271a.3216.3216 0 0 0 .1153-.6329c-.784-.1428-1.5271-.2125-2.216-.21zm11.0522.0176a.3216.3216 0 0 0-.0084.6432c1.8337.0239 3.1556.5956 3.7502 1.6256.8192 1.419.1798 3.5947-1.7182 5.837a.322.322 0 0 0 .0377.4535.3215.3215 0 0 0 .4532-.0377c2.0535-2.426 2.7708-4.8661 1.7845-6.5744-.7257-1.257-2.26-1.9207-4.299-1.9472zm-2.6984.2924a.3225.3225 0 0 0-.0647.0072c-1.8568.3979-3.8333 1.1755-5.7314 2.2714-4.5699 2.6384-7.5924 6.4948-7.3601 9.3717-.4726.2628-.7928.7664-.7928 1.3455 0 .85.6892 1.5392 1.5393 1.5392.85 0 1.5392-.6891 1.5392-1.5392 0-.8501-.6891-1.5393-1.5392-1.5393-.038 0-.0754.003-.1128.0057-.1002-2.5597 2.7434-6.1412 7.048-8.6265 1.8413-1.063 3.7551-1.8163 5.5445-2.1997a.3217.3217 0 0 0-.07-.636zm-2.8787 6.2364a1.1192 1.1192 0 0 0-.2243.0255c-.6012.1301-.983.7225-.8533 1.3238.1302.6012.7226.9832 1.3238.8533.6012-.1302.9832-.7226.8533-1.3238-.1139-.526-.5816-.8844-1.0995-.8788zM4.532 13.341a.321.321 0 0 0-.2318.0835.3214.3214 0 0 0-.0214.4542c1.2682 1.3936 2.9157 2.701 4.7946 3.7857 4.4146 2.5489 9.1056 3.2849 11.5608 1.8392a1.53 1.53 0 0 0 .8966.2899c.8501 0 1.5392-.6891 1.5392-1.5392 0-.8501-.689-1.5393-1.5392-1.5393-.85 0-1.5392.6892-1.5392 1.5393 0 .276.0737.5344.201.7584-2.2448 1.214-6.631.5002-10.7976-1.9054-1.8228-1.0524-3.418-2.3181-4.6404-3.6614a.3206.3206 0 0 0-.2226-.1049zm-2.0628 4.0172a.896.896 0 1 1 0 1.792.896.896 0 1 1 0-1.792zm19.0616 0a.896.896 0 1 1 0 1.792.891.891 0 0 1-.5864-.2194c-.0025-.004-.0039-.0083-.0066-.0123a.3195.3195 0 0 0-.0957-.0914.896.896 0 0 1 .6887-1.4689zm-14.0045 1.368a.3215.3215 0 0 0-.3207.4296C8.2793 22.154 10.036 24 12.0111 24c1.4406 0 2.7735-.9822 3.8128-2.711a.3215.3215 0 0 0-.11-.4413.3219.3219 0 0 0-.4415.11c-.934 1.5537-2.0812 2.399-3.2613 2.399-1.6407 0-3.2075-1.6465-4.2-4.4179a.3216.3216 0 0 0-.2848-.2126z", // Electron
  "flutter": "M14.314 0L2.3 12 6 15.7 21.684.013h-7.357zm.014 11.072L7.857 17.53l6.47 6.47H21.7l-6.46-6.468 6.46-6.46h-7.37z", // Flutter
  "go": "M1.811 10.231c-.047 0-.058-.023-.035-.059l.246-.315c.023-.035.081-.058.128-.058h4.172c.046 0 .058.035.035.07l-.199.303c-.023.036-.082.07-.117.07zM.047 11.306c-.047 0-.059-.023-.035-.058l.245-.316c.023-.035.082-.058.129-.058h5.328c.047 0 .07.035.058.07l-.093.28c-.012.047-.058.07-.105.07zm2.828 1.075c-.047 0-.059-.035-.035-.07l.163-.292c.023-.035.07-.07.117-.07h2.337c.047 0 .07.035.07.082l-.023.28c0 .047-.047.082-.082.082zm12.129-2.36c-.736.187-1.239.327-1.963.514-.176.046-.187.058-.34-.117-.174-.199-.303-.327-.548-.444-.737-.362-1.45-.257-2.115.175-.795.514-1.204 1.274-1.192 2.22.011.935.654 1.706 1.577 1.835.795.105 1.46-.175 1.987-.77.105-.13.198-.27.315-.434H10.47c-.245 0-.304-.152-.222-.35.152-.362.432-.97.596-1.274a.315.315 0 01.292-.187h4.253c-.023.316-.023.631-.07.947a4.983 4.983 0 01-.958 2.29c-.841 1.11-1.94 1.8-3.33 1.986-1.145.152-2.209-.07-3.143-.77-.865-.655-1.356-1.52-1.484-2.595-.152-1.274.222-2.419.993-3.424.83-1.086 1.928-1.776 3.272-2.02 1.098-.2 2.15-.07 3.096.571.62.41 1.063.97 1.356 1.648.07.105.023.164-.117.2m3.868 6.461c-1.064-.024-2.034-.328-2.852-1.029a3.665 3.665 0 01-1.262-2.255c-.21-1.32.152-2.489.947-3.529.853-1.122 1.881-1.706 3.272-1.95 1.192-.21 2.314-.095 3.33.595.923.63 1.496 1.484 1.648 2.605.198 1.578-.257 2.863-1.344 3.962-.771.783-1.718 1.273-2.805 1.495-.315.06-.63.07-.934.106zm2.78-4.72c-.011-.153-.011-.27-.034-.387-.21-1.157-1.274-1.81-2.384-1.554-1.087.245-1.788.935-2.045 2.033-.21.912.234 1.835 1.075 2.21.643.28 1.285.244 1.905-.07.923-.48 1.425-1.228 1.484-2.233z", // Go
  "npm": "M1.763 0C.786 0 0 .786 0 1.763v20.474C0 23.214.786 24 1.763 24h20.474c.977 0 1.763-.786 1.763-1.763V1.763C24 .786 23.214 0 22.237 0zM5.13 5.323l13.837.019-.009 13.836h-3.464l.01-10.382h-3.456L12.04 19.17H5.113z", // npm
  "python": "M14.25.18l.9.2.73.26.59.3.45.32.34.34.25.34.16.33.1.3.04.26.02.2-.01.13V8.5l-.05.63-.13.55-.21.46-.26.38-.3.31-.33.25-.35.19-.35.14-.33.1-.3.07-.26.04-.21.02H8.77l-.69.05-.59.14-.5.22-.41.27-.33.32-.27.35-.2.36-.15.37-.1.35-.07.32-.04.27-.02.21v3.06H3.17l-.21-.03-.28-.07-.32-.12-.35-.18-.36-.26-.36-.36-.35-.46-.32-.59-.28-.73-.21-.88-.14-1.05-.05-1.23.06-1.22.16-1.04.24-.87.32-.71.36-.57.4-.44.42-.33.42-.24.4-.16.36-.1.32-.05.24-.01h.16l.06.01h8.16v-.83H6.18l-.01-2.75-.02-.37.05-.34.11-.31.17-.28.25-.26.31-.23.38-.2.44-.18.51-.15.58-.12.64-.1.71-.06.77-.04.84-.02 1.27.05zm-6.3 1.98l-.23.33-.08.41.08.41.23.34.33.22.41.09.41-.09.33-.22.23-.34.08-.41-.08-.41-.23-.33-.33-.22-.41-.09-.41.09zm13.09 3.95l.28.06.32.12.35.18.36.27.36.35.35.47.32.59.28.73.21.88.14 1.04.05 1.23-.06 1.23-.16 1.04-.24.86-.32.71-.36.57-.4.45-.42.33-.42.24-.4.16-.36.09-.32.05-.24.02-.16-.01h-8.22v.82h5.84l.01 2.76.02.36-.05.34-.11.31-.17.29-.25.25-.31.24-.38.2-.44.17-.51.15-.58.13-.64.09-.71.07-.77.04-.84.01-1.27-.04-1.07-.14-.9-.2-.73-.25-.59-.3-.45-.33-.34-.34-.25-.34-.16-.33-.1-.3-.04-.25-.02-.2.01-.13v-5.34l.05-.64.13-.54.21-.46.26-.38.3-.32.33-.24.35-.2.35-.14.33-.1.3-.06.26-.04.21-.02.13-.01h5.84l.69-.05.59-.14.5-.21.41-.28.33-.32.27-.35.2-.36.15-.36.1-.35.07-.32.04-.28.02-.21V6.07h2.09l.14.01zm-6.47 14.25l-.23.33-.08.41.08.41.23.33.33.23.41.08.41-.08.33-.23.23-.33.08-.41-.08-.41-.23-.33-.33-.23-.41-.08-.41.08z", // Python
  "rust": "M23.8346 11.7033l-1.0073-.6236a13.7268 13.7268 0 00-.0283-.2936l.8656-.8069a.3483.3483 0 00-.1154-.578l-1.1066-.414a8.4958 8.4958 0 00-.087-.2856l.6904-.9587a.3462.3462 0 00-.2257-.5446l-1.1663-.1894a9.3574 9.3574 0 00-.1407-.2622l.49-1.0761a.3437.3437 0 00-.0274-.3361.3486.3486 0 00-.3006-.154l-1.1845.0416a6.7444 6.7444 0 00-.1873-.2268l.2723-1.153a.3472.3472 0 00-.417-.4172l-1.1532.2724a14.0183 14.0183 0 00-.2278-.1873l.0415-1.1845a.3442.3442 0 00-.49-.328l-1.076.491c-.0872-.0476-.1742-.0952-.2623-.1407l-.1903-1.1673A.3483.3483 0 0016.256.955l-.9597.6905a8.4867 8.4867 0 00-.2855-.086l-.414-1.1066a.3483.3483 0 00-.5781-.1154l-.8069.8666a9.2936 9.2936 0 00-.2936-.0284L12.2946.1683a.3462.3462 0 00-.5892 0l-.6236 1.0073a13.7383 13.7383 0 00-.2936.0284L9.9803.3374a.3462.3462 0 00-.578.1154l-.4141 1.1065c-.0962.0274-.1903.0567-.2855.086L7.744.955a.3483.3483 0 00-.5447.2258L7.009 2.348a9.3574 9.3574 0 00-.2622.1407l-1.0762-.491a.3462.3462 0 00-.49.328l.0416 1.1845a7.9826 7.9826 0 00-.2278.1873L3.8413 3.425a.3472.3472 0 00-.4171.4171l.2713 1.1531c-.0628.075-.1255.1509-.1863.2268l-1.1845-.0415a.3462.3462 0 00-.328.49l.491 1.0761a9.167 9.167 0 00-.1407.2622l-1.1662.1894a.3483.3483 0 00-.2258.5446l.6904.9587a13.303 13.303 0 00-.087.2855l-1.1065.414a.3483.3483 0 00-.1155.5781l.8656.807a9.2936 9.2936 0 00-.0283.2935l-1.0073.6236a.3442.3442 0 000 .5892l1.0073.6236c.008.0982.0182.1964.0283.2936l-.8656.8079a.3462.3462 0 00.1155.578l1.1065.4141c.0273.0962.0567.1914.087.2855l-.6904.9587a.3452.3452 0 00.2268.5447l1.1662.1893c.0456.088.0922.1751.1408.2622l-.491 1.0762a.3462.3462 0 00.328.49l1.1834-.0415c.0618.0769.1235.1528.1873.2277l-.2713 1.1541a.3462.3462 0 00.4171.4161l1.153-.2713c.075.0638.151.1255.2279.1863l-.0415 1.1845a.3442.3442 0 00.49.327l1.0761-.49c.087.0486.1741.0951.2622.1407l.1903 1.1662a.3483.3483 0 00.5447.2268l.9587-.6904a9.299 9.299 0 00.2855.087l.414 1.1066a.3452.3452 0 00.5781.1154l.8079-.8656c.0972.0111.1954.0203.2936.0294l.6236 1.0073a.3472.3472 0 00.5892 0l.6236-1.0073c.0982-.0091.1964-.0183.2936-.0294l.8069.8656a.3483.3483 0 00.578-.1154l.4141-1.1066a8.4626 8.4626 0 00.2855-.087l.9587.6904a.3452.3452 0 00.5447-.2268l.1903-1.1662c.088-.0456.1751-.0931.2622-.1407l1.0762.49a.3472.3472 0 00.49-.327l-.0415-1.1845a6.7267 6.7267 0 00.2267-.1863l1.1531.2713a.3472.3472 0 00.4171-.416l-.2713-1.1542c.0628-.0749.1255-.1508.1863-.2278l1.1845.0415a.3442.3442 0 00.328-.49l-.49-1.076c.0475-.0872.0951-.1742.1407-.2623l1.1662-.1893a.3483.3483 0 00.2258-.5447l-.6904-.9587.087-.2855 1.1066-.414a.3462.3462 0 00.1154-.5781l-.8656-.8079c.0101-.0972.0202-.1954.0283-.2936l1.0073-.6236a.3442.3442 0 000-.5892zm-6.7413 8.3551a.7138.7138 0 01.2986-1.396.714.714 0 11-.2997 1.396zm-.3422-2.3142a.649.649 0 00-.7715.5l-.3573 1.6685c-1.1035.501-2.3285.7795-3.6193.7795a8.7368 8.7368 0 01-3.6951-.814l-.3574-1.6684a.648.648 0 00-.7714-.499l-1.473.3158a8.7216 8.7216 0 01-.7613-.898h7.1676c.081 0 .1356-.0141.1356-.088v-2.536c0-.074-.0536-.0881-.1356-.0881h-2.0966v-1.6077h2.2677c.2065 0 1.1065.0587 1.394 1.2088.0901.3533.2875 1.5044.4232 1.8729.1346.413.6833 1.2381 1.2685 1.2381h3.5716a.7492.7492 0 00.1296-.0131 8.7874 8.7874 0 01-.8119.9526zM6.8369 20.024a.714.714 0 11-.2997-1.396.714.714 0 01.2997 1.396zM4.1177 8.9972a.7137.7137 0 11-1.304.5791.7137.7137 0 011.304-.579zm-.8352 1.9813l1.5347-.6824a.65.65 0 00.33-.8585l-.3158-.7147h1.2432v5.6025H3.5669a8.7753 8.7753 0 01-.2834-3.348zm6.7343-.5437V8.7836h2.9601c.153 0 1.0792.1772 1.0792.8697 0 .575-.7107.7815-1.2948.7815zm10.7574 1.4862c0 .2187-.008.4363-.0243.651h-.9c-.09 0-.1265.0586-.1265.1477v.413c0 .973-.5487 1.1846-1.0296 1.2382-.4576.0517-.9648-.1913-1.0275-.4717-.2704-1.5186-.7198-1.8436-1.4305-2.4034.8817-.5599 1.799-1.386 1.799-2.4915 0-1.1936-.819-1.9458-1.3769-2.3153-.7825-.5163-1.6491-.6195-1.883-.6195H5.4682a8.7651 8.7651 0 014.907-2.7699l1.0974 1.151a.648.648 0 00.9182.0213l1.227-1.1743a8.7753 8.7753 0 016.0044 4.2762l-.8403 1.8982a.652.652 0 00.33.8585l1.6178.7188c.0283.2875.0425.577.0425.8717zm-9.3006-9.5993a.7128.7128 0 11.984 1.0316.7137.7137 0 01-.984-1.0316zm8.3389 6.71a.7107.7107 0 01.9395-.3625.7137.7137 0 11-.9405.3635z", // Rust
  "tauri": "M13.912 0a8.72 8.72 0 0 0-8.308 6.139c1.05-.515 2.18-.845 3.342-.976 2.415-3.363 7.4-3.412 9.88-.097 2.48 3.315 1.025 8.084-2.883 9.45a6.131 6.131 0 0 1-.3 2.762 8.72 8.72 0 0 0 3.01-1.225A8.72 8.72 0 0 0 13.913 0zm.082 6.451a2.284 2.284 0 1 0-.15 4.566 2.284 2.284 0 0 0 .15-4.566zm-5.629.27a8.72 8.72 0 0 0-3.031 1.235 8.72 8.72 0 1 0 13.06 9.9131 10.173 10.174 0 0 1-3.343.965 6.125 6.125 0 1 1-7.028-9.343 6.114 6.115 0 0 1 .342-2.772zm1.713 6.27a2.284 2.284 0 0 0-2.284 2.283 2.284 2.284 0 0 0 2.284 2.284 2.284 2.284 0 0 0 2.284-2.284 2.284 2.284 0 0 0-2.284-2.284z", // Tauri
  "xcode": "M19.06 5.3327c.4517-.1936.7744-.2581 1.097-.1936.5163.1291.7744.5163.968.7098.1936.3872.9034.7744 1.2261.8389.2581.0645.7098-.6453 1.0325-1.2906.3227-.5808.5163-1.3552.4517-1.5488-.0645-.1936-.968-.5808-1.1616-.5808-.1291 0-.3872.1291-.8389.0645-.4517-.0645-.9034-.5808-1.1616-.968-.4517-.6453-1.097-1.0325-1.6778-1.3552-.6453-.3227-1.3552-.5163-2.065-.6453-1.0325-.2581-2.065-.4517-3.0975-.3227-.5808.0645-1.2906.1291-1.8069.3227-.0645 0-.1936.1936-.0645.1936s.5808.0645.5808.0645-.5807.1292-.5807.2583c0 .1291.0645.1291.1291.1291.0645 0 1.4842-.0645 2.065 0 .6453.1291 1.3552.4517 1.8069 1.2261.7744 1.4197.4517 2.7749.2581 3.2266-.968 2.1295-8.6472 15.2294-9.0344 16.1328-.3873.9034-.5163 1.4842.5807 2.065s1.6778.3227 2.0005-.0645c.3872-.5163 7.0339-17.1654 9.2925-18.2624zm-3.6138 8.7117h1.5488c1.0325 0 1.2261.5163 1.2261.7098.0645.5163-.1936 1.1616-1.2261 1.1616h-.968l.7744 1.2906c.4517.7744.2581 1.1616 0 1.4197-.3872.3872-1.2261.3872-1.6778-.4517l-.9034-1.5488c-.6453 1.4197-1.2906 2.9684-2.065 4.7753h4.0009c1.9359 0 3.5492-1.6133 3.5492-3.5492V6.5588c-.0645-.1291-.1936-.0645-.2581 0-.3872.4517-1.4842 2.0004-4.001 7.4856zm-9.8087 8.0019h-.3227c-2.3231 0-4.1945-1.8714-4.1945-4.1945V7.0105c0-2.3231 1.8714-4.1945 4.1945-4.1945h9.3571c-.1936-.1936-.968-.5163-1.7423-.4517-.3227 0-.968.1291-1.3552-.1291-.3872-.3227-.3227-.5163-.9034-.5163H4.9277c-2.6458 0-4.7753 2.1295-4.7753 4.7753v11.7447c0 2.6458 2.1295 4.7753 4.4527 4.7108.6452 0 .8388-.5162 1.0324-.9034zM20.4152 6.9459v10.9058c0 2.3231-1.8714 4.1945-4.1945 4.1945H11.897s-.3872 1.0325.8389 1.0325h3.8719c2.6458 0 4.7753-2.1295 4.7753-4.7753V8.8173c.0646-.9034-.7098-1.4842-.9679-1.8714zm-18.5851.0646v10.8413c0 1.9359 1.6133 3.5492 3.5492 3.5492h.5808c0-.0645.7744-1.4197 2.4522-4.2591.1936-.3872.4517-.7744.7098-1.2261H4.4114c-.5808 0-.9034-.3872-.968-.7098-.1291-.5163.1936-1.1616.9034-1.1616h2.3877l3.033-5.2916s-.7098-1.2906-.9034-1.6133c-.2582-.4517-.1291-.9034.129-1.1615.3872-.3872 1.0325-.5808 1.6778.4517l.2581.3872.2581-.3872c.5808-.8389.968-.7744 1.2906-.7098.5163.1291.8389.7098.3872 1.6133L8.864 14.0444h1.3552c.4517-.7744.9034-1.5488 1.3552-2.3877-.0645-.3227-.1291-.7098-.0645-1.0325.0645-.5163.3227-.968.6453-1.3552l.3872.6453c1.2261-2.1295 2.1295-3.9364 2.3877-4.6463.1291-.3872.3227-1.1616.1291-1.8069H5.3794c-2.0005.0001-3.5493 1.6134-3.5493 3.5494zM4.605 17.7872c0-.0645.7744-1.4197.7744-1.4197 1.2261-.3227 1.8069.4517 1.8714.5163 0 0-.8389 1.4842-1.097 1.7423s-.5808.3227-.9034.2581c-.5164-.129-.839-.6453-.6454-1.097z", // Xcode
};

// Each adapter's mark and brand color (`light`: the color needs a dark glyph).
const TOOL_MARKS = {
  cargo: { logo: "rust", color: "#b7410e" },
  go: { logo: "go", color: "#007d9c" },
  npm: { logo: "npm", color: "#cb3837" },
  cmake: { logo: "cmake", color: "#064f8c" },
  gradle: { logo: "android", color: "#3ddc84", light: true },
  flutter: { logo: "flutter", color: "#02569b" },
  xcode: { logo: "xcode", color: "#147efb" },
  maven: { logo: "apachemaven", color: "#c71a36" },
  python: { logo: "python", color: "#3776ab" },
  tauri: { logo: "tauri", color: "#24c8d8", light: true },
  electron: { logo: "electron", color: "#47848f" },
  docker: { logo: "docker", color: "#2496ed" },
  dotnet: { logo: "dotnet", color: "#512bd4" },
};

function toolMark(adapter) {
  const mark = TOOL_MARKS[adapter];
  const tile = el("span", `tool-mark${mark && mark.light ? " on-light" : ""}`);
  tile.setAttribute("aria-hidden", "true");
  // Through the CSSOM (allowed by the CSP), not a style attribute.
  tile.style.setProperty("--tool", mark ? mark.color : "#6b7280");
  if (mark) {
    const svg = svgEl("svg", { viewBox: "0 0 24 24" });
    svg.append(svgEl("path", { d: LOGOS[mark.logo] }));
    tile.append(svg);
  } else {
    tile.append(el("span", "tool-letters", adapter.slice(0, 2).toUpperCase()));
  }
  return tile;
}

function toolBadge(adapter, withLabel = true) {
  const label = (TOOL_LABELS[adapter] || ["", adapter])[1];
  const wrap = el("span", "tool");
  wrap.append(toolMark(adapter));
  if (withLabel) wrap.append(el("span", "", label));
  else wrap.setAttribute("title", label);
  return wrap;
}

// ---------- derived data ----------

function outcome(build) {
  if (!build.succeeded) return "failed";
  const ran = build.steps.some((s) => s.status === "succeeded");
  const reused = build.steps.some((s) => s.status === "reused");
  return reused && !ran ? "reused" : "built";
}

const OUTCOME_LABEL = { built: "Built", reused: "Reused", failed: "Failed" };

function statusBadge(build) {
  const kind = outcome(build);
  return el("span", `status ${kind}`, OUTCOME_LABEL[kind]);
}

function matchesQuery(build) {
  if (!state.query) return true;
  const q = state.query.toLowerCase();
  const tool = (TOOL_LABELS[build.adapter] || ["", build.adapter])[1].toLowerCase();
  return build.project.toLowerCase().includes(q) || build.adapter.includes(q) || tool.includes(q);
}

function visibleBuilds() {
  return state.builds.filter(matchesQuery);
}

// Time saved by reuse: for each fully reused build, the duration of the
// most recent earlier successful build of the same project that really ran,
// minus what the reuse took. An estimate, labeled as such.
function timeSaved(builds) {
  let saved = 0;
  let counted = 0;
  const ordered = [...builds].sort((a, b) => a.started_ms - b.started_ms);
  const lastRun = new Map();
  for (const build of ordered) {
    const kind = outcome(build);
    if (kind === "built") lastRun.set(build.project, build.millis);
    else if (kind === "reused" && lastRun.has(build.project)) {
      saved += Math.max(0, lastRun.get(build.project) - build.millis);
      counted += 1;
    }
  }
  return { saved, counted };
}

function projects(builds) {
  const map = new Map();
  for (const build of builds) {
    if (!map.has(build.project)) map.set(build.project, []);
    map.get(build.project).push(build);
  }
  return [...map.entries()].map(([path, items]) => ({ path, items, latest: items[0] }));
}

function currentFiles(builds) {
  const files = [];
  for (const build of builds) {
    for (const file of build.downloads) {
      if (!file.replaced) files.push({ file, build });
    }
  }
  return files;
}

// ---------- charts ----------

function sparkline(svg, values, { failed = [] } = {}) {
  svg.replaceChildren();
  if (values.length === 0) return;
  const width = 120;
  const height = 32;
  const max = Math.max(...values, 1);
  const step = values.length > 1 ? width / (values.length - 1) : 0;
  const points = values.map((v, i) => [values.length > 1 ? i * step : width / 2, height - 3 - (v / max) * (height - 6)]);
  const line = points.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
  svg.append(svgEl("path", { class: "area", d: `${line} L${points[points.length - 1][0].toFixed(1)} ${height} L${points[0][0].toFixed(1)} ${height} Z` }));
  svg.append(svgEl("path", { class: "line", d: line }));
  failed.forEach((isFailed, i) => {
    if (isFailed) svg.append(svgEl("circle", { class: "point-failed", cx: points[i][0], cy: points[i][1], r: 2.6 }));
  });
}

function activityChart(builds, now) {
  const svg = $("chart");
  svg.replaceChildren();
  const days = 14;
  const startOfToday = new Date(now);
  startOfToday.setHours(0, 0, 0, 0);
  const buckets = Array.from({ length: days }, (_, i) => ({
    start: startOfToday.getTime() - (days - 1 - i) * DAY, built: 0, reused: 0, failed: 0,
  }));
  for (const build of builds) {
    const index = Math.floor((build.started_ms - buckets[0].start) / DAY);
    if (index >= 0 && index < days) buckets[index][outcome(build)] += 1;
  }
  const W = 640, H = 220, left = 32, right = 8, top = 10, bottom = 26;
  const plotW = W - left - right, plotH = H - top - bottom;
  const peak = Math.max(1, ...buckets.map((b) => b.built + b.reused + b.failed));
  const niceMax = peak <= 4 ? 4 : Math.ceil(peak / 4) * 4;
  for (let i = 0; i <= 4; i += 1) {
    const y = top + plotH - (plotH * i) / 4;
    svg.append(svgEl("line", { class: "grid", x1: left, x2: W - right, y1: y, y2: y }));
    const label = svgEl("text", { class: "axis", x: left - 8, y: y + 4, "text-anchor": "end" });
    label.textContent = String((niceMax * i) / 4);
    svg.append(label);
  }
  const slot = plotW / days;
  const barW = Math.min(26, slot * 0.62);
  let total = 0;
  buckets.forEach((bucket, i) => {
    const x = left + i * slot + (slot - barW) / 2;
    let y = top + plotH;
    for (const kind of ["built", "reused", "failed"]) {
      const count = bucket[kind];
      if (!count) continue;
      total += count;
      const h = (count / niceMax) * plotH;
      y -= h;
      const rect = svgEl("rect", { class: `bar-${kind}`, x: x.toFixed(1), y: y.toFixed(1), width: barW.toFixed(1), height: Math.max(h - 1, 1).toFixed(1), rx: 3 });
      const title = svgEl("title");
      title.textContent = `${new Date(bucket.start).toLocaleDateString()}: ${count} ${OUTCOME_LABEL[kind].toLowerCase()}`;
      rect.append(title);
      svg.append(rect);
    }
    if (i % 2 === (days - 1) % 2) {
      const label = svgEl("text", { class: i === days - 1 ? "axis today" : "axis", x: (x + barW / 2).toFixed(1), y: H - 8, "text-anchor": "middle" });
      label.textContent = i === days - 1 ? "Today" : new Date(bucket.start).toLocaleDateString(undefined, { month: "short", day: "numeric" });
      svg.append(label);
    }
  });
  $("chart-desc").textContent = `${total} builds in the last 14 days.`;
}

// ---------- tables ----------

function buildsTable(table, builds, { expandable = false, limit = Infinity } = {}) {
  table.replaceChildren();
  const head = el("thead");
  const headRow = el("tr");
  const columns = [
    ...(expandable ? [["", ""]] : []),
    ["Status", ""], ["Project", ""], ["Toolchain", "hide-sm"], ["Duration", "num"],
    ["Steps", "hide-sm"], ["Started", "num"],
  ];
  for (const [label, cls] of columns) {
    const th = el("th", cls, label);
    th.scope = "col";
    if (!label) th.append(el("span", "sr-only", "Details"));
    headRow.append(th);
  }
  head.append(headRow);
  const body = el("tbody");
  const shown = builds.slice(0, limit);
  if (shown.length === 0) {
    const row = el("tr");
    const cell = el("td", "none", state.builds.length ? "No builds match." : "No builds yet.");
    cell.colSpan = columns.length;
    row.append(cell);
    body.append(row);
  }
  for (const build of shown) {
    const row = el("tr", "row");
    const detailId = `detail-${build.id}`;
    if (expandable) {
      const cell = el("td");
      const button = el("button", "expand");
      button.type = "button";
      const open = state.expanded.has(build.id);
      button.setAttribute("aria-expanded", String(open));
      button.setAttribute("aria-controls", detailId);
      button.dataset.build = build.id;
      button.setAttribute("aria-label", `Details for the ${projectName(build.project)} build from ${new Date(build.started_ms).toLocaleString()}`);
      button.append(icon("m9 6 6 6-6 6"));
      button.addEventListener("click", () => {
        if (state.expanded.has(build.id)) state.expanded.delete(build.id);
        else state.expanded.add(build.id);
        render();
      });
      cell.append(button);
      row.append(cell);
    }
    const status = el("td");
    status.append(statusBadge(build));
    row.append(status);
    const project = el("td", "cell-project", projectName(build.project));
    project.append(el("span", "cell-sub", build.flavor || build.adapter));
    row.append(project);
    const tool = el("td", "hide-sm");
    tool.append(toolBadge(build.adapter));
    row.append(tool);
    row.append(el("td", "num", duration(build.millis)));
    const ran = build.steps.filter((s) => s.status === "succeeded").length;
    const reused = build.steps.filter((s) => s.status === "reused").length;
    const failed = build.steps.filter((s) => s.status !== "succeeded" && s.status !== "reused").length;
    const parts = [];
    if (ran) parts.push(`${ran} ran`);
    if (reused) parts.push(`${reused} reused`);
    if (failed) parts.push(`${failed} failed`);
    row.append(el("td", "hide-sm muted", parts.join(" · ") || "–"));
    const started = el("td", "num muted");
    const time = el("time", "", ago(build.started_ms, state.now));
    time.dateTime = new Date(build.started_ms).toISOString();
    time.title = new Date(build.started_ms).toLocaleString();
    started.append(time);
    row.append(started);
    body.append(row);
    if (expandable && state.expanded.has(build.id)) body.append(detailRow(build, columns.length, detailId));
  }
  table.append(head, body);
}

function detailRow(build, span, id) {
  const row = el("tr", "detail");
  row.id = id;
  const cell = el("td");
  cell.colSpan = span;
  const grid = el("div", "detail-grid");
  const stepsBox = el("div");
  stepsBox.append(el("h3", "", "Steps"));
  const steps = el("ul", "steps");
  for (const step of build.steps) {
    const item = el("li", "step");
    const kind = step.status === "succeeded" ? "built" : step.status === "reused" ? "reused" : "failed";
    const label = { succeeded: "Ran", reused: "Reused", failed: "Failed", skipped: "Skipped", timed_out: "Timed out", cancelled: "Cancelled" }[step.status] || step.status;
    item.append(el("span", `status ${kind}`, label), el("span", "step-name", step.id), el("span", "step-time", step.status === "reused" ? "from cache" : duration(step.millis)));
    steps.append(item);
  }
  if (!build.steps.length) steps.append(el("li", "muted", "No steps ran."));
  stepsBox.append(steps);
  const filesBox = el("div");
  filesBox.append(el("h3", "", "Deliverables"));
  filesBox.append(fileList(build.downloads.map((file) => ({ file, build })), true));
  const where = el("p", "cell-sub", build.project);
  stepsBox.append(where);
  grid.append(stepsBox, filesBox);
  cell.append(grid);
  const output = outputSection(build.id);
  output.classList.add("detail-output");
  cell.append(output);
  row.append(cell);
  return row;
}

function fileList(entries, compact = false) {
  const list = el("ul", compact ? "steps" : "files");
  if (!entries.length) list.append(el("li", compact ? "muted" : "none", compact ? "None collected." : "Nothing to download yet."));
  for (const { file, build } of entries) {
    const item = el("li", compact ? "step" : "file");
    const extension = (file.name.split(".").pop() || "file").slice(0, 4);
    if (!compact) item.append(el("span", "file-type", file.folder ? "dir" : file.name.includes(".") ? extension : "bin"));
    const body = el("div", "file-body");
    body.append(el("span", compact ? "step-name" : "file-name", file.name));
    if (!compact) {
      const size = file.exists && !file.replaced ? `${bytes(file.bytes)} · ` : "";
      body.append(el("span", "file-meta", `${size}${projectName(build.project)} · ${build.adapter}${build.flavor ? ` · ${build.flavor}` : ""} · built ${ago(build.started_ms, state.now)}`));
    }
    item.append(body);
    if (file.replaced) {
      item.append(el("span", "file-gone", "Replaced by a newer build"));
    } else if (!file.exists) {
      item.append(el("span", "file-gone", "No longer on disk"));
    } else if (file.folder) {
      // A whole folder (a web app, a published app): open it from disk.
      item.append(el("span", "file-note", compact ? `Folder · ${bytes(file.bytes)}` : `Folder in ${file.path.split(/[\\/]/)[0]}/`));
    } else {
      const link = el("a", compact ? "link" : "btn primary");
      link.href = file.url;
      link.setAttribute("download", file.name);
      link.setAttribute("aria-label", `Download ${file.name}, ${bytes(file.bytes)}`);
      if (!compact) link.append(icon("M12 3v11m0 0-4-4m4 4 4-4M5 17v2a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-2"));
      link.append(document.createTextNode(compact ? `Download · ${bytes(file.bytes)}` : "Download"));
      if (!compact && file.name.toLowerCase().endsWith(".apk")) {
        const sharing = activeShare && activeShare.file === file.name;
        const phone = el("button", "btn", sharing ? "Sharing · show code" : "Install on phone");
        phone.type = "button";
        phone.dataset.share = file.url;
        phone.prepend(icon("M8 2h8a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2zm3 17h2"));
        phone.setAttribute("aria-label", sharing ? `Show the install code for ${file.name}` : `Install ${file.name} on a phone`);
        phone.addEventListener("click", () => {
          if (sharing) showShare(activeShare, null);
          else startShare(build.id, Number(file.url.split("/").pop()), file.name);
        });
        item.append(phone);
      }
      item.append(link);
    }
    list.append(item);
  }
  return list;
}

// ---------- pages ----------

function renderOverview(builds) {
  const now = state.now;
  const week = builds.filter((b) => b.started_ms >= now - 7 * DAY);
  const previous = builds.filter((b) => b.started_ms < now - 7 * DAY && b.started_ms >= now - 14 * DAY);
  $("kpi-builds").textContent = String(week.length);
  $("kpi-builds-sub").textContent = previous.length
    ? `${week.length >= previous.length ? "+" : ""}${week.length - previous.length} vs previous 7 days`
    : `${builds.length} in history`;
  const perDay = Array.from({ length: 7 }, (_, i) => {
    const start = now - (7 - i) * DAY;
    return week.filter((b) => b.started_ms >= start && b.started_ms < start + DAY).length;
  });
  sparkline($("spark-builds"), perDay);

  const ok = builds.filter((b) => b.succeeded).length;
  $("kpi-success").textContent = percent(ok, builds.length);
  $("kpi-success-sub").textContent = builds.length ? `${builds.length - ok} failed of ${builds.length}` : "No builds yet";
  $("meter-success").style.width = builds.length ? `${(ok / builds.length) * 100}%` : "0";

  const steps = builds.flatMap((b) => b.steps);
  const reused = steps.filter((s) => s.status === "reused").length;
  $("kpi-reuse").textContent = percent(reused, steps.length);
  $("kpi-reuse-sub").textContent = steps.length ? `${reused} of ${steps.length} steps came from the cache` : "No steps yet";
  $("meter-reuse").style.width = steps.length ? `${(reused / steps.length) * 100}%` : "0";

  const { saved, counted } = timeSaved(builds);
  $("kpi-saved").textContent = counted ? duration(saved) : "–";
  $("kpi-saved-sub").textContent = counted
    ? `≈ across ${counted} reused build${counted === 1 ? "" : "s"}, vs. each project's last real build`
    : "Shows once a build is reused";

  activityChart(builds, now);

  const list = $("overview-projects");
  list.replaceChildren();
  const top = projects(builds).slice(0, 6);
  if (!top.length) list.append(el("li", "none", "No projects yet."));
  for (const project of top) {
    const item = el("li");
    const button = el("button", "mini-project");
    button.type = "button";
    button.append(toolBadge(project.latest.adapter, false));
    const body = el("span", "mini-body");
    body.append(el("span", "mini-name", projectName(project.path)));
    body.append(el("span", "mini-meta", `${project.items.length} build${project.items.length === 1 ? "" : "s"} · ${percent(project.items.filter((b) => b.succeeded).length, project.items.length)} succeeded`));
    button.append(body, statusBadge(project.latest), el("span", "mini-time", ago(project.latest.started_ms, now)));
    button.addEventListener("click", () => showProject(project.path));
    item.append(button);
    list.append(item);
  }

  buildsTable($("overview-builds"), builds, { limit: 6 });
}

// ---------- live builds ----------

function serverNow() {
  return Date.now() + state.clockOffset;
}

function renderRunning() {
  const card = $("live-builds");
  const list = $("running-list");
  list.replaceChildren();
  card.hidden = state.running.length === 0;
  for (const build of state.running) {
    const item = el("li", "running-item");
    const top = el("div", "running-top");
    top.append(toolBadge(build.adapter, false));
    const title = el("div", "running-title");
    title.append(el("span", "running-name", projectName(build.project)));
    const done = build.steps.length;
    const total = Math.max(build.total_steps, done + build.current.length, 1);
    const now = build.current.length ? `now: ${build.current.join(", ")}` : "finishing";
    title.append(el("span", "running-meta",
      `${build.adapter}${build.flavor ? ` · ${build.flavor}` : ""} · step ${Math.min(done + 1, total)} of ${total} · ${now}`));
    top.append(title);
    const timer = el("span", "running-timer", duration(Math.max(0, serverNow() - build.started_ms)));
    timer.dataset.since = String(build.started_ms);
    timer.setAttribute("aria-label", "Elapsed time");
    top.append(timer);
    item.append(top);
    const bar = el("div", "meter meter-brand");
    bar.setAttribute("role", "progressbar");
    bar.setAttribute("aria-label", `${projectName(build.project)} progress`);
    bar.setAttribute("aria-valuemin", "0");
    bar.setAttribute("aria-valuemax", String(total));
    const fill = el("span");
    if (done === 0) {
      // Nothing finished yet: show activity rather than a bar stuck at 0%.
      bar.classList.add("indeterminate");
      bar.setAttribute("aria-valuetext", "In progress");
    } else {
      bar.setAttribute("aria-valuenow", String(done));
      fill.style.width = `${(done / total) * 100}%`;
    }
    bar.append(fill);
    item.append(bar);
    if (build.steps.length) {
      const steps = el("p", "running-steps",
        build.steps.map((s) => `${s.id} ${s.status === "reused" ? "reused" : s.status === "succeeded" ? duration(s.millis) : s.status}`).join(" · "));
      item.append(steps);
    }
    item.append(outputSection(build.id, { live: true }));
    list.append(item);
  }
}

// Elapsed timers tick every second without re-rendering anything else.
setInterval(() => {
  updateShareExpiry();
  for (const timer of document.querySelectorAll("[data-since]")) {
    timer.textContent = duration(Math.max(0, serverNow() - Number(timer.dataset.since)));
  }
}, 1000);

// ---------- build output (D-045) ----------

const ESC = String.fromCharCode(27);
const BEL = String.fromCharCode(7);
// Control sequences other than colors (cursor moves, erase, titles).
const OTHER_ESCAPES = new RegExp(
  `${ESC}\\[[0-9;?]*[A-Za-ln-z]|${ESC}\\][^${BEL}${ESC}]*(?:${BEL}|${ESC}\\\\)|${ESC}[()][0-9A-Za-z]|${ESC}[=>78]`, "g");
const SGR = new RegExp(`${ESC}\\[([0-9;]*)m`, "g");

// Apply one SGR parameter list to a style state {fg, bold, dim}.
function applySgr(style, params) {
  const codes = params === "" ? [0] : params.split(";").map(Number);
  for (let i = 0; i < codes.length; i += 1) {
    const code = codes[i];
    if (code === 0) { style.fg = null; style.bold = false; style.dim = false; }
    else if (code === 1) style.bold = true;
    else if (code === 2) style.dim = true;
    else if (code === 22) { style.bold = false; style.dim = false; }
    else if ((code >= 30 && code <= 37) || (code >= 90 && code <= 97)) style.fg = code;
    else if (code === 39) style.fg = null;
    else if (code === 38 || code === 48) i += codes[i + 1] === 5 ? 2 : codes[i + 1] === 2 ? 4 : 0;
  }
}

// Render one line of terminal output into `target` (text nodes and spans
// with classes only; never HTML). Returns the style state after the line.
function renderTerminalLine(target, raw, style) {
  target.replaceChildren();
  let text = raw.replace(/\r+$/, "");
  const lastReturn = text.lastIndexOf("\r");
  if (lastReturn >= 0) text = text.slice(lastReturn + 1); // progress redraws
  text = text.replace(OTHER_ESCAPES, "");
  const current = { ...style };
  let last = 0;
  const push = (chunk) => {
    if (!chunk) return;
    if (current.fg === null && !current.bold && !current.dim) {
      target.append(document.createTextNode(chunk));
    } else {
      const classes = [];
      if (current.fg !== null) classes.push(`a${current.fg}`);
      if (current.bold) classes.push("ab");
      if (current.dim) classes.push("ad");
      target.append(el("span", classes.join(" "), chunk));
    }
  };
  for (const match of text.matchAll(SGR)) {
    push(text.slice(last, match.index));
    applySgr(current, match[1]);
    last = match.index + match[0].length;
  }
  push(text.slice(last));
  if (!target.childNodes.length) target.append(document.createTextNode(" "));
  return current;
}

class LogView {
  constructor(id) {
    this.id = id;
    this.next = 0;
    this.carry = "";
    this.style = { fg: null, bold: false, dim: false };
    this.lines = 0;
    this.loading = false;
    this.missing = false;
    this.pre = el("pre", "log");
    this.pre.tabIndex = 0;
    this.pre.setAttribute("aria-label", "Build output");
    this.body = el("div");
    this.partial = el("div", "log-line");
    this.status = el("div", "log-status", "Loading output…");
    this.pre.append(this.status, this.body, this.partial);
  }

  append(text) {
    const pre = this.pre;
    const atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 12;
    this.carry += text;
    const parts = this.carry.split("\n");
    this.carry = parts.pop();
    for (const part of parts) {
      const line = el("div", "log-line");
      this.style = renderTerminalLine(line, part, this.style);
      this.body.append(line);
      this.lines += 1;
      if (this.lines > 5000) {
        this.body.firstChild.remove();
        this.lines -= 1;
      }
    }
    renderTerminalLine(this.partial, this.carry, this.style);
    if (atBottom) pre.scrollTop = pre.scrollHeight;
  }

  // Fetch everything new (in chunks), then stop until asked again.
  async load() {
    if (this.loading) return;
    this.loading = true;
    try {
      for (let round = 0; round < 40; round += 1) {
        const response = await fetch(`api/log/${encodeURIComponent(this.id)}?from=${this.next}`, { cache: "no-store" });
        if (!response.ok) {
          if (this.next === 0) {
            this.missing = true;
            this.status.textContent = "No output was recorded for this build (it ran before output capture, or its log was pruned).";
          }
          break;
        }
        const data = await response.json();
        if (data.text) this.append(data.text);
        this.next = data.next;
        this.status.textContent = this.next === 0 ? "No output yet." : "";
        this.status.hidden = this.next > 0;
        if (this.next >= data.size) break;
      }
    } catch {
      this.status.textContent = "Could not load the output.";
    } finally {
      this.loading = false;
    }
  }
}

// The output of a job on a server, read through Ant (D-065).
class RemoteLogView extends LogView {
  constructor(server, job) {
    super(`server:${server}:${job}`);
    this.remote = true;
    this.server = server;
    this.job = job;
    this.pre.setAttribute("aria-label", `Output of ${job} on ${server}`);
  }

  async load() {
    if (this.loading) return;
    this.loading = true;
    try {
      for (let round = 0; round < 40; round += 1) {
        const url = `api/servers/${encodeURIComponent(this.server)}/jobs/${encodeURIComponent(this.job)}/log?from=${this.next}`;
        const answer = await (await fetch(url, { cache: "no-store" })).json();
        if (!answer.ok) {
          this.status.textContent = answer.error;
          this.status.hidden = false;
          break;
        }
        const data = answer.data;
        if (data.text) this.append(data.text);
        const moved = data.next !== this.next;
        this.next = data.next;
        this.status.textContent = this.next === 0 ? "No output yet." : "";
        this.status.hidden = this.next > 0;
        if (!moved || !data.text) break;
      }
    } catch (error) {
      this.status.textContent = `Could not load the output: ${error.message}`;
      this.status.hidden = false;
    } finally {
      this.loading = false;
    }
  }
}

function remoteLogView(server, job) {
  const key = `server:${server}:${job}`;
  if (!logViews.has(key)) logViews.set(key, new RemoteLogView(server, job));
  return logViews.get(key);
}

const logViews = new Map();
const openLogs = new Set();

function logView(id) {
  if (!logViews.has(id)) logViews.set(id, new LogView(id));
  return logViews.get(id);
}

// A "Show output" toggle plus the viewer; the viewer element is reused
// across re-renders so scroll position and content survive.
function outputSection(id, { live = false } = {}) {
  const box = el("div", "output");
  const toggle = el("button", "btn", openLogs.has(id) ? "Hide output" : live ? "Show live output" : "Show output");
  toggle.type = "button";
  toggle.setAttribute("aria-expanded", String(openLogs.has(id)));
  toggle.dataset.log = id;
  toggle.addEventListener("click", () => {
    if (openLogs.has(id)) openLogs.delete(id);
    else {
      openLogs.add(id);
      logView(id).load();
    }
    render();
  });
  box.append(toggle);
  if (openLogs.has(id)) box.append(logView(id).pre);
  return box;
}

// Refresh open viewers of running builds (called by the poll loop).
function followLiveLogs() {
  for (const build of state.running) {
    if (openLogs.has(build.id)) logView(build.id).load();
  }
}

function renderBuilds(builds) {
  const scoped = state.project ? builds.filter((b) => b.project === state.project) : builds;
  const counts = { all: scoped.length, built: 0, reused: 0, failed: 0 };
  for (const build of scoped) counts[outcome(build)] += 1;
  for (const key of Object.keys(counts)) $(`count-${key}`).textContent = String(counts[key]);
  for (const chip of document.querySelectorAll(".chip")) {
    chip.setAttribute("aria-pressed", String(chip.dataset.filter === state.filter));
  }
  const filtered = state.filter === "all" ? scoped : scoped.filter((b) => outcome(b) === state.filter);
  const scope = $("builds-scope");
  scope.replaceChildren();
  if (state.project) {
    scope.append(document.createTextNode(`Only ${projectName(state.project)} · `));
    const clear = el("button", "link", "show all projects");
    clear.type = "button";
    clear.classList.add("chip-clear");
    clear.addEventListener("click", () => {
      state.project = null;
      render();
    });
    scope.append(clear);
  } else {
    scope.textContent = "Newest first · select a row's arrow for steps and downloads";
  }
  buildsTable($("builds-table"), filtered, { expandable: true });
}

function renderProjects(builds) {
  const grid = $("project-grid");
  grid.replaceChildren();
  const all = projects(builds);
  if (!all.length) {
    grid.append(el("p", "none card", state.builds.length ? "No projects match." : "No projects yet."));
    return;
  }
  for (const project of all) {
    const card = el("article", "card project-card");
    const top = el("div", "project-top");
    top.append(toolBadge(project.latest.adapter, false));
    const title = el("div", "project-title");
    title.append(el("h2", "", projectName(project.path)));
    const path = el("p", "project-path", project.path);
    path.title = project.path;
    title.append(path);
    top.append(title, statusBadge(project.latest));
    card.append(top);

    const stats = el("div", "project-stats");
    const ok = project.items.filter((b) => b.succeeded);
    const typical = ok.filter((b) => outcome(b) === "built").map((b) => b.millis).sort((a, b) => a - b);
    for (const [label, value] of [
      ["Builds", String(project.items.length)],
      ["Success", percent(ok.length, project.items.length)],
      ["Typical build", typical.length ? duration(typical[Math.floor(typical.length / 2)]) : "–"],
    ]) {
      const stat = el("div", "stat");
      stat.append(el("p", "stat-label", label), el("p", "stat-value", value));
      stats.append(stat);
    }
    card.append(stats);

    const recent = project.items.slice(0, 20).reverse();
    const spark = svgEl("svg", { class: "project-spark", viewBox: "0 0 120 32", preserveAspectRatio: "none", "aria-hidden": "true" });
    sparkline(spark, recent.map((b) => b.millis), { failed: recent.map((b) => !b.succeeded) });
    card.append(spark);

    const foot = el("div", "project-foot");
    foot.append(el("span", "count-pill", `${(TOOL_LABELS[project.latest.adapter] || ["", project.latest.adapter])[1]} · last build ${ago(project.latest.started_ms, state.now)}`));
    const view = el("button", "btn", "View builds");
    view.type = "button";
    view.addEventListener("click", () => showProject(project.path));
    foot.append(view);
    card.append(foot);
    grid.append(card);
  }
}

// ---------- install on phone (D-046) ----------

let activeShare = null; // { file, url, qr, expires_ms } while sharing

async function shareRequest(path, body) {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Ant-Dashboard": "1" },
    body: body ? JSON.stringify(body) : "",
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.json();
}

function drawQr(svg, rows) {
  svg.replaceChildren();
  const size = rows.length;
  const quiet = 4;
  const total = size + quiet * 2;
  svg.setAttribute("viewBox", `0 0 ${total} ${total}`);
  svg.append(svgEl("rect", { x: 0, y: 0, width: total, height: total, class: "qr-light" }));
  rows.forEach((row, y) => {
    // One rectangle per run of dark modules keeps the drawing small.
    let x = 0;
    while (x < row.length) {
      if (row[x] !== "1") { x += 1; continue; }
      let end = x;
      while (end < row.length && row[end] === "1") end += 1;
      svg.append(svgEl("rect", { x: x + quiet, y: y + quiet, width: end - x, height: 1, class: "qr-dark" }));
      x = end;
    }
  });
}

function showShare(share, error) {
  const dialog = $("share-dialog");
  activeShare = share;
  $("share-error").hidden = !error;
  $("share-error").textContent = error || "";
  $("share-body").hidden = !share;
  $("share-stop").hidden = !share;
  $("share-url").parentElement.hidden = !share;
  $("share-firewall").hidden = !share || !/Windows/i.test(navigator.userAgent);
  if (share) {
    $("share-file").textContent = share.file;
    drawQr($("share-qr"), share.qr);
    $("share-url").textContent = share.url;
    updateShareExpiry();
  } else {
    $("share-expiry").textContent = "";
  }
  if (!dialog.open) dialog.showModal();
  render();
}

function updateShareExpiry() {
  if (!activeShare) return;
  const left = activeShare.expires_ms - serverNow();
  if (left <= 0) {
    activeShare = null;
    $("share-expiry").textContent = "Sharing has stopped.";
    $("share-body").hidden = true;
    $("share-stop").hidden = true;
    render();
    return;
  }
  const minutes = Math.floor(left / 60000);
  const seconds = Math.floor((left % 60000) / 1000);
  $("share-expiry").textContent =
    `Sharing on your network for ${minutes}:${String(seconds).padStart(2, "0")} more. Only someone with this link can download it.`;
}

async function startShare(build, index, file) {
  $("share-file").textContent = file;
  try {
    const data = await shareRequest("api/share", { build, index });
    showShare(data.share, data.error);
  } catch (error) {
    showShare(null, `Could not start sharing (${error.message}). Is \`ant dashboard\` still running?`);
  }
}

$("share-stop").addEventListener("click", async () => {
  try { await shareRequest("api/share/stop"); } catch { /* the share expires on its own */ }
  activeShare = null;
  $("share-dialog").close();
  render();
});
$("share-done").addEventListener("click", () => $("share-dialog").close());
$("share-close").addEventListener("click", () => $("share-dialog").close());

// A share started earlier (another tab, or before a reload) is shown too.
fetch("api/share", { cache: "no-store" })
  .then((response) => (response.ok ? response.json() : null))
  .then((data) => { if (data && data.share) { activeShare = data.share; render(); } })
  .catch(() => {});

function renderDeliverables(builds) {
  const target = $("files");
  const entries = currentFiles(builds);
  target.replaceWith(Object.assign(fileList(entries), { id: "files" }));
}

function showProject(path) {
  state.project = path;
  state.filter = "all";
  location.hash = "#builds";
  render();
}

// ---------- shell ----------

const TITLES = { overview: "Overview", builds: "Builds", projects: "Projects", deliverables: "Deliverables", servers: "Servers" };

// ---------- servers (D-053) ----------

const servers = { list: [], loaded: false, live: {} }; // live: name -> { busy, ok, error, data }
const SERVER_POLL_MS = 2000;
let serverTimer = null;

async function loadServers() {
  try {
    const response = await fetch("api/servers", { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    servers.list = (await response.json()).servers;
  } catch {
    servers.list = [];
  }
  servers.loaded = true;
  $("nav-servers").textContent = servers.list.length ? String(servers.list.length) : "";
  if (state.page === "servers") {
    render();
    pollServers();
  }
}

// While the Servers page is open, read each server's workers and jobs
// through Ant (which keeps one SSH tunnel per server and never hands the
// browser a token), and follow open job logs (D-065).
async function pollServers() {
  clearTimeout(serverTimer);
  if (state.page !== "servers") return;
  await Promise.all(servers.list.map((server) => loadServerLive(server.name)));
  for (const [key, view] of logViews) {
    if (view.remote && openLogs.has(key) && remoteJobActive(view)) view.load();
  }
  serverTimer = setTimeout(pollServers, SERVER_POLL_MS);
}

async function loadServerLive(name) {
  const live = servers.live[name] || (servers.live[name] = { busy: false, ok: false, error: null, data: null });
  if (live.busy) return;
  live.busy = true;
  try {
    const response = await fetch(`api/servers/${encodeURIComponent(name)}/live`, { cache: "no-store" });
    const answer = await response.json();
    const before = liveSignature(live);
    if (answer.ok) Object.assign(live, { ok: true, error: null, data: answer.data });
    else Object.assign(live, { ok: false, error: answer.error });
    // Redraw only when something visible changed: redrawing every poll
    // would swallow clicks. Running jobs' timers tick in place.
    if (liveSignature(live) !== before) render();
    else tickServerTimers(name, live);
  } catch (error) {
    Object.assign(live, { ok: false, error: `Could not ask Ant: ${error.message}` });
    render();
  } finally {
    live.busy = false;
  }
}

function workerOnline(worker, now) {
  // A busy worker does not poll for work, so it is online while it builds.
  return Boolean(worker.current_job) || now - worker.last_seen_unix_ms < 10000;
}

function liveSignature(live) {
  const data = live.data;
  if (!data) return JSON.stringify([live.ok, live.error]);
  const now = data.workers.now_unix_ms;
  return JSON.stringify([
    live.ok, live.error,
    data.workers.workers.map((w) => [w.worker_id, w.current_job, w.jobs_reported, workerOnline(w, now)]),
    data.jobs.jobs.map((j) => [j.job_id, j.state, j.succeeded, j.worker_id, j.finished_unix_ms]),
    Math.floor(now / 60000), // relative times move once a minute
  ]);
}

function tickServerTimers(name, live) {
  const now = live.data?.workers?.now_unix_ms;
  if (!now) return;
  for (const cell of document.querySelectorAll("td.took[data-started]")) {
    if (cell.dataset.server === name) cell.textContent = duration(now - Number(cell.dataset.started));
  }
}

function remoteJobActive(view) {
  const jobs = servers.live[view.server]?.data?.jobs?.jobs || [];
  const job = jobs.find((j) => j.job_id === view.job);
  return !job || job.state !== "completed" || view.next === 0;
}

function jobStatus(job) {
  if (job.state === "queued") return el("span", "status queued", "Queued");
  if (job.state === "running") return el("span", "status running", "Running");
  if (job.reused) return el("span", "status reused", "Reused");
  return job.succeeded ? el("span", "status built", "Succeeded") : el("span", "status failed", "Failed");
}

function serverTile(label, value, tone) {
  const tile = el("div", `server-tile${tone ? ` ${tone}` : ""}`);
  tile.append(el("span", "server-tile-value", String(value)), el("span", "server-tile-label", label));
  return tile;
}

function renderServerLive(server) {
  const live = servers.live[server.name];
  const box = el("div", "server-live");
  if (!live || (!live.data && !live.error)) {
    box.append(el("p", "server-note", "Connecting over SSH…"));
    return box;
  }
  if (live.error) {
    box.append(el("p", "server-status bad", live.error));
    if (!live.data) return box;
  }
  const now = live.data.workers.now_unix_ms;
  const jobs = live.data.jobs.jobs;
  const workers = live.data.workers.workers;
  const online = workers.filter((w) => workerOnline(w, now));
  const failed = jobs.filter((j) => j.state === "completed" && !j.succeeded).length;
  const tiles = el("div", "server-tiles");
  tiles.append(
    serverTile("Workers online", online.length, online.length ? "ok" : "bad"),
    serverTile("Running", jobs.filter((j) => j.state === "running").length, "brand"),
    serverTile("Queued", jobs.filter((j) => j.state === "queued").length),
    serverTile("Succeeded", jobs.filter((j) => j.state === "completed" && j.succeeded).length),
    serverTile("Failed", failed, failed ? "bad" : ""),
  );
  box.append(tiles);

  box.append(el("h3", "server-section", "Workers"));
  const workerList = el("ul", "worker-list");
  if (!workers.length) workerList.append(el("li", "none", "No worker has checked in yet."));
  for (const worker of workers) {
    const isOnline = workerOnline(worker, now);
    const item = el("li", "worker");
    const body = el("div", "file-body");
    body.append(
      el("span", "worker-name", worker.worker_id),
      el("span", "file-meta", worker.current_job ? `Building ${worker.current_job}`
        : isOnline ? "Idle, ready for work" : `Last seen ${ago(worker.last_seen_unix_ms, now)}`),
    );
    const badge = worker.current_job ? el("span", "status running", "Busy")
      : isOnline ? el("span", "status built", "Online") : el("span", "status failed", "Offline");
    item.append(body, el("span", "worker-count", `${worker.jobs_reported} done`), badge);
    workerList.append(item);
  }
  box.append(workerList);

  const jobsHead = el("div", "server-section-row");
  jobsHead.append(el("h3", "server-section", "Jobs"),
    el("span", "server-note", `Kept since the server started ${ago(live.data.jobs.coordinator_started_unix_ms, now)}`));
  box.append(jobsHead);
  if (!jobs.length) {
    const empty = el("p", "server-empty");
    empty.append(document.createTextNode("No jobs yet. Build here with "));
    empty.append(el("code", "", `ant build --on ${server.name}`));
    empty.append(document.createTextNode(" and every step shows up live."));
    box.append(empty);
    return box;
  }
  const wrap = el("div", "table-wrap server-jobs");
  const table = el("table", "table");
  const head = el("thead");
  const headRow = el("tr");
  for (const label of ["Step", "Status", "Worker", "Started", "Took", "Output"]) headRow.append(el("th", "", label));
  head.append(headRow);
  const body = el("tbody");
  for (const job of jobs.slice(0, 50)) {
    const key = `server:${server.name}:${job.job_id}`;
    const row = el("tr", "row");
    const step = el("td");
    step.append(el("span", "job-action", job.action_id), el("span", "job-id", job.job_id));
    const statusCell = el("td");
    statusCell.append(jobStatus(job));
    if (job.error) statusCell.append(el("span", "job-error", job.error));
    const started = job.started_unix_ms || job.submitted_unix_ms;
    const took = job.started_unix_ms ? (job.finished_unix_ms || now) - job.started_unix_ms : null;
    const tookCell = el("td", "num took", took === null ? "—" : duration(took));
    if (job.state === "running" && job.started_unix_ms) {
      tookCell.dataset.server = server.name;
      tookCell.dataset.started = String(job.started_unix_ms);
    }
    const outputCell = el("td");
    const open = openLogs.has(key);
    const toggle = el("button", "btn", open ? "Hide" : job.state === "running" ? "Watch live" : "Show");
    toggle.type = "button";
    toggle.dataset.log = key;
    toggle.setAttribute("aria-expanded", String(open));
    toggle.setAttribute("aria-label", `${open ? "Hide" : "Show"} the output of ${job.action_id}`);
    toggle.addEventListener("click", () => {
      if (openLogs.has(key)) openLogs.delete(key);
      else {
        openLogs.add(key);
        remoteLogView(server.name, job.job_id).load();
      }
      render();
    });
    outputCell.append(toggle);
    row.append(step, statusCell, el("td", "muted", job.worker_id || "—"), el("td", "muted", ago(started, now)),
      tookCell, outputCell);
    body.append(row);
    if (open) {
      const logRow = el("tr", "log-row");
      const cell = el("td");
      cell.colSpan = 6;
      cell.append(remoteLogView(server.name, job.job_id).pre);
      logRow.append(cell);
      body.append(logRow);
    }
  }
  table.append(head, body);
  wrap.append(table);
  box.append(wrap);
  return box;
}

function renderServers() {
  const list = $("servers-list");
  list.replaceChildren();
  if (!servers.loaded) {
    list.append(el("li", "none", "Loading…"));
    return;
  }
  if (!servers.list.length) {
    const item = el("li", "none");
    item.append(document.createTextNode("No servers yet. Have a Linux server? Run "));
    item.append(el("code", "", "ant server deploy you@your-server"));
    item.append(document.createTextNode(" in a terminal: it installs Ant there over SSH, and nothing is exposed to the internet."));
    list.append(item);
    return;
  }
  for (const server of servers.list) {
    const item = el("li", "server");
    const top = el("div", "server-top");
    const glyph = el("span", "server-icon");
    glyph.append(icon("M4 5h16v6H4zm0 8h16v6H4zm3-5h.01M7 16h.01"));
    const body = el("div", "file-body");
    body.append(el("span", "file-name", server.name));
    const platform = [server.os, server.arch].filter(Boolean).join(" · ");
    body.append(el("span", "file-meta", `${server.target}${platform ? ` · ${platform}` : ""} · deployed ${ago(server.deployed_ms, state.now)}`));
    const live = servers.live[server.name];
    const badge = !live || (!live.data && !live.error) ? el("span", "status queued", "Connecting")
      : live.ok ? el("span", "status built", "Connected") : el("span", "status failed", "Unreachable");
    top.append(glyph, body, badge);
    item.append(top, renderServerLive(server));
    list.append(item);
  }
}

// Re-rendering replaces elements; keep keyboard focus on the "same" control
// (identified by its id, data attributes, or label) across renders.
function focusKey(node) {
  if (!node || node === document.body) return null;
  return node.id ? `#${node.id}`
    : node.dataset.build ? `[data-build="${CSS.escape(node.dataset.build)}"]`
    : node.dataset.log ? `[data-log="${CSS.escape(node.dataset.log)}"]`
    : node.dataset.server ? `[data-server="${CSS.escape(node.dataset.server)}"]`
    : node.dataset.share ? `[data-share="${CSS.escape(node.dataset.share)}"]`
    : node.getAttribute("aria-label") ? `[aria-label="${CSS.escape(node.getAttribute("aria-label"))}"]`
      : node.dataset.filter ? `[data-filter="${node.dataset.filter}"]`
        : null;
}

function render() {
  const key = focusKey(document.activeElement);
  renderPage();
  state.renderedAt = Date.now();
  if (key && document.activeElement === document.body) {
    const again = document.querySelector(key);
    if (again) again.focus({ preventScroll: true });
  }
}

function renderPage() {
  const builds = visibleBuilds();
  const page = state.page;
  $("page-title").textContent = TITLES[page];
  document.title = `${TITLES[page]} · Ant`;
  for (const link of document.querySelectorAll(".nav a")) {
    if (link.dataset.page === page) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  $("nav-builds").textContent = state.builds.length ? String(state.builds.length) : "";
  $("nav-projects").textContent = state.builds.length ? String(projects(state.builds).length) : "";
  const fileCount = currentFiles(state.builds).filter(({ file }) => file.exists).length;
  $("nav-deliverables").textContent = fileCount ? String(fileCount) : "";

  renderRunning();
  // The Servers page does not depend on build history.
  const empty = page !== "servers" && state.loaded && state.builds.length === 0 && state.running.length === 0;
  $("empty").hidden = !empty;
  for (const name of Object.keys(TITLES)) $(`page-${name}`).hidden = empty || name !== page;
  if (page === "servers") renderServers();
  if (empty) return;
  if (page === "overview") renderOverview(builds);
  if (page === "builds") renderBuilds(builds);
  if (page === "projects") renderProjects(builds);
  if (page === "deliverables") renderDeliverables(builds);
}

function route() {
  const page = location.hash.replace("#", "");
  state.page = TITLES[page] ? page : "overview";
  closeMenu();
  if (state.page === "servers") loadServers();
  render();
}

async function refresh() {
  try {
    const response = await fetch("api/builds", { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json();
    state.builds = data.builds;
    state.running = data.running || [];
    state.now = data.now_ms;
    state.clockOffset = data.now_ms - Date.now();
    state.lastOk = Date.now();
    state.loaded = true;
    if (state.project && !state.builds.some((b) => b.project === state.project)) state.project = null;
    $("live").classList.remove("bad");
    $("live-text").textContent = state.running.length ? `${state.running.length} building` : "Up to date";
    $("banner").hidden = true;
    // Only re-render when something changed, or once a minute so relative
    // times move: rebuilding the page every few seconds would steal focus.
    const signature = JSON.stringify([
      data.builds.map((b) => [b.id, b.downloads.map((f) => [f.exists, f.replaced, f.bytes])]),
      state.running.map((b) => [b.id, b.steps.length, b.current]),
    ]);
    if (signature !== state.signature || Date.now() - state.renderedAt > 60000) {
      state.signature = signature;
      render();
    }
  } catch (error) {
    $("live").classList.add("bad");
    $("live-text").textContent = "Offline";
    const banner = $("banner");
    banner.textContent = `Lost contact with Ant${state.lastOk ? ` (last update ${ago(state.lastOk, Date.now())})` : ""}. Is \`ant dashboard\` still running in your terminal?`;
    banner.hidden = false;
  }
}

// Theme: follows the system unless the viewer picked one (remembered in
// this browser only; storage can be unavailable, which is fine).
function applyTheme(theme) {
  if (theme) document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}
function currentTheme() {
  return document.documentElement.dataset.theme
    || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
}
try { applyTheme(localStorage.getItem("ant-theme")); } catch { /* storage unavailable */ }
$("theme").addEventListener("click", () => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  applyTheme(next);
  try { localStorage.setItem("ant-theme", next); } catch { /* storage unavailable */ }
});

function openMenu() {
  $("sidebar").classList.add("open");
  $("scrim").hidden = false;
  $("menu").setAttribute("aria-expanded", "true");
}
function closeMenu() {
  $("sidebar").classList.remove("open");
  $("scrim").hidden = true;
  $("menu").setAttribute("aria-expanded", "false");
}
$("menu").addEventListener("click", () => ($("sidebar").classList.contains("open") ? closeMenu() : openMenu()));
$("scrim").addEventListener("click", closeMenu);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeMenu();
  if (event.key === "/" && document.activeElement !== $("search")) {
    event.preventDefault();
    $("search").focus();
  }
});

$("search").addEventListener("input", (event) => {
  state.query = event.target.value.trim();
  render();
});
for (const chip of document.querySelectorAll(".chip")) {
  chip.addEventListener("click", () => {
    state.filter = chip.dataset.filter;
    render();
  });
}
window.addEventListener("hashchange", route);

// Poll every 3 s, every second while something is building.
async function poll() {
  await refresh();
  followLiveLogs();
  setTimeout(poll, state.running.length ? 1000 : REFRESH_MS);
}

route();
poll();
