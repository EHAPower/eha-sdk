// Copyright The eha-sdk Contributors

// The raw document is authoritative.  The small parser below exposes JSON
// leaf spans without converting numeric tokens to JavaScript Numbers, so an
// untouched u64 (or future wider integer) remains byte-for-byte exportable.
const ws = (raw, index) => { while (/\s/.test(raw[index] || "")) index++; return index; };
const stringEnd = (raw, start) => {
  let escaped = false;
  for (let index = start + 1; index < raw.length; index++) {
    if (!escaped && raw[index] === '"') return index + 1;
    escaped = !escaped && raw[index] === "\\";
    if (raw[index] !== "\\") escaped = false;
  }
  throw new Error("JSON 字符串未闭合");
};
const tokenEnd = (raw, start) => {
  let index = start;
  while (index < raw.length && !/[\s,}\]]/.test(raw[index])) index++;
  return index;
};
const jsonNumber = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/;

// 控制与试验的 f32 在 Rust 边界才量化。浏览器只检查 JSON 数字语法，
// 并在请求体中逐字输出用户输入，避免 Number/JSON.stringify 的双重舍入。
export class RawJsonNumber {
  constructor(raw) { this.raw = raw; }
}

export function rawJsonNumber(value, label, { optional = false } = {}) {
  const raw = String(value ?? "").trim();
  if (!raw && optional) return null;
  if (!jsonNumber.test(raw)) throw new Error(`${label} 必须是 JSON 数值字面量。`);
  return new RawJsonNumber(raw);
}

export function stringifyJson(value) {
  if (value instanceof RawJsonNumber) return value.raw;
  if (value === null || typeof value === "string" || typeof value === "boolean" || typeof value === "number") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map((entry) => entry === undefined ? "null" : stringifyJson(entry)).join(",")}]`;
  if (typeof value === "object") return `{${Object.entries(value).filter(([, entry]) => entry !== undefined).map(([key, entry]) => `${JSON.stringify(key)}:${stringifyJson(entry)}`).join(",")}}`;
  return JSON.stringify(value);
}

export function parseJsonTree(raw) {
  // This parser keeps token spans, but JSON.parse remains the grammar
  // authority. An arbitrary bare token must not appear valid merely because
  // it is delimited by a comma.
  JSON.parse(raw);
  function value(at) {
    const start = ws(raw, at); const first = raw[start];
    if (first === '"') return { start, end:stringEnd(raw, start), kind:"string" };
    if (first !== "{" && first !== "[") return { start, end:tokenEnd(raw, start), kind:"literal" };
    const object = first === "{"; let index = ws(raw, start + 1); const children = new Map();
    if (raw[index] === (object ? "}" : "]")) return { start, end:index + 1, kind:object ? "object" : "array", children };
    let item = 0;
    while (index < raw.length) {
      let key = String(item++);
      if (object) {
        if (raw[index] !== '"') throw new Error("对象键必须是字符串");
        const end = stringEnd(raw, index); key = JSON.parse(raw.slice(index, end)); index = ws(raw, end);
        if (raw[index] !== ":") throw new Error("对象键后缺少冒号"); index++;
      }
      const child = value(index); children.set(key, child); index = ws(raw, child.end);
      if (raw[index] === (object ? "}" : "]")) return { start, end:index + 1, kind:object ? "object" : "array", children };
      if (raw[index] !== ",") throw new Error("JSON 项之间缺少逗号");
      index = ws(raw, index + 1);
    }
    throw new Error("JSON 容器未闭合");
  }
  const root = value(0); if (ws(raw, root.end) !== raw.length) throw new Error("JSON 尾部包含额外内容"); return root;
}

export function literalAt(raw, pointer) {
  let node = parseJsonTree(raw);
  for (const part of pointer.split("/").slice(1).map((entry) => entry.replace(/~1/g, "/").replace(/~0/g, "~"))) {
    node = node.children?.get(part); if (!node) return null;
  }
  return raw.slice(node.start, node.end);
}

export function replaceLiteral(raw, pointer, literal) {
  JSON.parse(`{"value":${literal}}`);
  let node = parseJsonTree(raw);
  for (const part of pointer.split("/").slice(1).map((entry) => entry.replace(/~1/g, "/").replace(/~0/g, "~"))) {
    node = node.children?.get(part); if (!node) throw new Error(`配置中没有字段 ${pointer}`);
  }
  return raw.slice(0, node.start) + literal + raw.slice(node.end);
}

export function diffPointers(baseline, candidate) {
  const walk = (raw, node, pointer, output) => {
    if (!node.children) { output.set(pointer, raw.slice(node.start, node.end)); return; }
    for (const [key, child] of node.children) walk(raw, child, `${pointer}/${key.replace(/~/g, "~0").replace(/\//g, "~1")}`, output);
  };
  const before = new Map(); const after = new Map(); walk(baseline, parseJsonTree(baseline), "", before); walk(candidate, parseJsonTree(candidate), "", after);
  return [...new Set([...before.keys(), ...after.keys()])].filter((pointer) => before.get(pointer) !== after.get(pointer)).map((pointer) => ({ pointer, before:before.get(pointer), after:after.get(pointer) }));
}

function schemaLeaves(schema) {
  const resolve = (entry) => entry.$ref ? resolve(schema.$defs?.[entry.$ref.split("/").at(-1)] || {}) : entry;
  const output = [];
  const visit = (entry, pointer = "", title = "") => {
    entry = resolve(entry);
    if (entry.type === "object" || entry.properties) for (const [key, child] of Object.entries(entry.properties || {})) visit(child, `${pointer}/${key}`, title ? `${title} / ${key}` : key);
    else output.push({ pointer, title, ...entry });
  };
  visit(schema); return output;
}

export function createConfigEditor({ get, onChange, sourceKey }) {
  const fields = document.querySelector("#config-fields"); const state = document.querySelector("#config-schema-state"); const diff = document.querySelector("#config-diff"); const record = document.querySelector("#config-record");
  const jsonState = document.querySelector("#config-json-state");
  let schema; let baseline = ""; let documentSource = null;
  const setJsonState = (text = "", invalid = false) => {
    if (jsonState) { jsonState.textContent = text; jsonState.classList.toggle("is-error", invalid); }
    if (invalid) record.setAttribute("aria-invalid", "true");
    else record.removeAttribute("aria-invalid");
  };
  const setFieldValue = (input, leaf, literal) => { input.value = leaf.type === "string" ? JSON.parse(literal) : literal; };
  const redraw = () => {
    if (!record.value) { fields.replaceChildren(); diff.textContent = "读取或导入完整 JSON 后显示字段。"; setJsonState(); return; }
    try { parseJsonTree(record.value); }
    catch (error) {
      fields.replaceChildren();
      diff.textContent = "草稿 JSON 尚不能生成字段表单或差异；原始内容已保留，可继续修正后校验。";
      state.textContent = `无法生成表单：${error.message}`;
      setJsonState(`JSON 无效：${error.message}。字段表单和差异已暂停；修正后可继续校验。`, true);
      return;
    }
    setJsonState();
    if (!schema) { fields.replaceChildren(); diff.textContent = "Schema 尚未取得。"; return; }
    const changes = baseline ? diffPointers(baseline, record.value) : [];
    state.textContent = "已加载当前 SDK Schema；表单不拥有第二份字段规则。";
    diff.replaceChildren();
    const summary = document.createElement("p"); summary.textContent = !baseline ? "尚无设备基线；读取配置后才能比较字段差异。" : changes.length ? `相对本次基线已修改 ${changes.length} 个字段。` : "与本次基线没有字段差异。"; diff.append(summary);
    if (changes.length) { const list = document.createElement("details"); const title = document.createElement("summary"); title.textContent = "查看字段差异"; list.append(title); for (const change of changes) { const row = document.createElement("p"); row.textContent = `${change.pointer || "/"}：${change.before ?? "（缺失）"} → ${change.after ?? "（缺失）"}`; list.append(row); } diff.append(list); }
    const livePointers = new Set();
    for (const leaf of schemaLeaves(schema)) {
      const literal = literalAt(record.value, leaf.pointer); if (literal === null) continue;
      livePointers.add(leaf.pointer);
      let label = [...fields.children].find((candidate) => candidate.dataset.pointer === leaf.pointer);
      let input = label?.querySelector("input, select");
      const needsReplacement = !label || (leaf.enum ? input?.tagName !== "SELECT" : input?.tagName !== "INPUT");
      if (needsReplacement) {
        label?.remove();
        label = document.createElement("label"); label.className = "schema-field"; label.dataset.pointer = leaf.pointer;
        const heading = document.createElement("span"); heading.dataset.fieldTitle = ""; label.append(heading);
        input = document.createElement(leaf.enum ? "select" : "input"); input.dataset.pointer = leaf.pointer;
        input.addEventListener("change", () => {
          const next = leaf.type === "string" || leaf.enum ? JSON.stringify(input.value) : input.value.trim();
          try { record.value = replaceLiteral(record.value, leaf.pointer, next); onChange(record.value, documentSource); redraw(); }
          catch (error) {
            state.textContent = `字段未应用：${error.message}`;
            const current = literalAt(record.value, leaf.pointer);
            if (current !== null) setFieldValue(input, leaf, current);
          }
        });
        const help = document.createElement("small"); help.dataset.fieldHelp = ""; label.append(input, help);
      }
      label.querySelector("[data-field-title]").textContent = leaf.title;
      if (leaf.enum) {
        if (input.options.length !== leaf.enum.length || [...input.options].some((option, index) => option.value !== String(leaf.enum[index]))) {
          input.replaceChildren(...leaf.enum.map((optionValue) => new Option(optionValue, optionValue)));
        }
        input.value = JSON.parse(literal);
      } else {
        input.type = "text"; input.inputMode = leaf.type === "integer" || leaf.type === "number" ? "decimal" : "text";
        setFieldValue(input, leaf, literal);
      }
      const constraints = [leaf.minimum !== undefined ? `最小 ${leaf.minimum}` : null, leaf.maximum !== undefined ? `最大 ${leaf.maximum}` : null, leaf.exclusiveMinimum !== undefined ? `必须大于 ${leaf.exclusiveMinimum}` : null].filter(Boolean);
      const help = label.querySelector("[data-field-help]");
      help.textContent = [leaf.description, constraints.join("；")].filter(Boolean).join(" ");
      help.hidden = !help.textContent;
      if (needsReplacement) fields.append(label);
    }
    [...fields.children].filter((label) => !livePointers.has(label.dataset.pointer)).forEach((label) => label.remove());
  };
  record.addEventListener("input", () => { onChange(record.value, documentSource); redraw(); });
  document.querySelector("#config-import").addEventListener("click", () => document.querySelector("#config-file").click());
  document.querySelector("#config-file").addEventListener("change", async (event) => { const file = event.target.files?.[0]; if (!file) return; try { record.value = await file.text(); baseline = ""; documentSource = null; onChange(record.value, null, baseline); redraw(); } catch (error) { state.textContent = `导入失败：${error.message}`; } finally { event.target.value = ""; } });
  document.querySelector("#config-export").addEventListener("click", () => { const url = URL.createObjectURL(new Blob([record.value], { type:"application/json;charset=utf-8" })); const link = document.createElement("a"); link.href = url; link.download = "eha-config.json"; link.click(); URL.revokeObjectURL(url); });
  return {
    async loadSchema() { try { const body = await get("/api/config/schema"); schema = body.schema || body; state.textContent = "已加载当前 SDK Schema；表单不拥有第二份字段规则。"; redraw(); } catch (error) { state.textContent = `Schema 不可用：${error.message}`; } },
    setDocument(raw, source, nextBaseline = raw) { record.value = raw; baseline = nextBaseline; documentSource = source; redraw(); },
    sourceChanged() { if (documentSource && documentSource !== sourceKey()) state.textContent = "此草稿来自另一设备或运行实例；保存前将再次确认。"; },
    redraw,
  };
}
