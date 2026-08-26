import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const required = [
  "AGENTS.md",
  "ARCHITECTURE.md",
  "README.md",
  "docs/DEVELOPMENT.md",
  "docs/INSTALL.md",
  "docs/architecture/library.md",
  "docs/architecture/lastfm-import-matching.md",
  "docs/architecture/spotify.md",
  "docs/architecture/playback.md",
  "docs/architecture/persistence.md",
];
const retired = ["docs/PLAN.md", "docs/LOCAL_FILES_PLAN.md", "docs/VIEWS_PLAN.md"];
const errors = [];

for (const file of required) {
  if (!fs.existsSync(path.join(root, file))) errors.push(`missing ${file}`);
}
for (const file of retired) {
  if (fs.existsSync(path.join(root, file))) errors.push(`retired file remains: ${file}`);
}

const walkMarkdown = (directory) => fs.readdirSync(directory, { withFileTypes: true })
  .flatMap((entry) => entry.isDirectory()
    ? walkMarkdown(path.join(directory, entry.name))
    : entry.name.endsWith(".md") ? [path.join(directory, entry.name)] : []);
const markdown = ["AGENTS.md", "ARCHITECTURE.md", "README.md", "apps/desktop/README.md",
  ...walkMarkdown(path.join(root, "docs")).map((file) => path.relative(root, file))];

for (const file of markdown) {
  const absolute = path.join(root, file);
  const contents = fs.readFileSync(absolute, "utf8");
  for (const match of contents.matchAll(/\[[^\]]*\]\(([^)]+)\)/g)) {
    const link = match[1].split("#", 1)[0];
    if (!link || /^(?:https?:|mailto:)/.test(link)) continue;
    const target = path.resolve(path.dirname(absolute), decodeURIComponent(link));
    if (!fs.existsSync(target)) errors.push(`${file}: broken link ${match[1]}`);
  }
}

const agents = fs.readFileSync(path.join(root, "AGENTS.md"), "utf8");
for (const file of ["ARCHITECTURE.md", ...markdown.filter((file) => file.startsWith("docs/"))]) {
  if (!agents.includes(file)) errors.push(`AGENTS.md does not route to ${file}`);
}

const install = fs.readFileSync(path.join(root, "docs/INSTALL.md"), "utf8");
const normalizedInstall = install.replace(/\s+/g, " ");
const version = JSON.parse(fs.readFileSync(path.join(root, "apps/desktop/src-tauri/tauri.conf.json"), "utf8")).version;
for (const match of install.matchAll(/(?:Retune v|Retune-|retune_|\/(?:tag|download)\/v)(\d+\.\d+\.\d+)/g)) {
  if (match[1] !== version) errors.push(`docs/INSTALL.md: stale version ${match[1]} (expected ${version})`);
}
for (const value of [
  "brew install --cask open-cli-collective/tap/retune",
  "winget install --exact --id OpenCLICollective.Retune",
  "sudo apt install retune",
  "http://127.0.0.1:8898/callback",
  "Do **not** register",
  "/login",
  "stable-signed",
  "not Apple-notarized",
  "Unknown",
  "SmartScreen",
  "Linux Secret Service",
  "checksums.txt",
]) {
  if (!install.includes(value)) errors.push(`docs/INSTALL.md: missing contract ${value}`);
}
for (const value of [
  "Do **not** register `http://127.0.0.1:8898/login`; `/login` is Retune's separate internal built-in-playback callback.",
  "Retune uses Authorization Code with PKCE, so it does not need or store the client secret.",
  "approve the separate one-time **Authorize Spotify playback** prompt",
  "Spotify Development Mode currently requires the application owner to have Premium and limits each app to five authenticated users.",
  "Anyone other than the owner must be added to the app allowlist",
  "You can import and play local files while signed out.",
]) {
  if (!normalizedInstall.includes(value)) errors.push(`docs/INSTALL.md: missing relationship ${value}`);
}

if (errors.length) {
  console.error(errors.join("\n"));
  process.exit(1);
}
console.log(`Documentation checks passed (${markdown.length} files).`);
