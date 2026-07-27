import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const required = [
  "AGENTS.md",
  "ARCHITECTURE.md",
  "README.md",
  "docs/DEVELOPMENT.md",
  "docs/architecture/library.md",
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

if (errors.length) {
  console.error(errors.join("\n"));
  process.exit(1);
}
console.log(`Documentation checks passed (${markdown.length} files).`);
