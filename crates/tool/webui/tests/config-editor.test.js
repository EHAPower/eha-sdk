// Copyright The eha-sdk Contributors
import test from "node:test";
import assert from "node:assert/strict";
import { diffPointers, literalAt, parseJsonTree, rawJsonNumber, replaceLiteral, stringifyJson } from "../static/config-editor.js";

test("控制请求将 f32 中点小数字面量原样写入 HTTP JSON", () => {
  const midpoint = "1.0000000596046447753906250000000000000000000001";
  const body = stringifyJson({ action:"position", mm:rawJsonNumber(midpoint, "目标位置") });
  assert.equal(body, `{"action":"position","mm":${midpoint}}`);
  assert.match(body, new RegExp(`"mm":${midpoint.replace(/[.]/g, "\\.")}`));
});

test("试验请求将 f32 中点小数字面量原样写入嵌套 HTTP JSON", () => {
  const midpoint = "1.0000000596046447753906250000000000000000000001";
  const body = stringifyJson({ action:"start", trial:{ command:{ action:"position", mm:rawJsonNumber(midpoint, "目标位置") } } });
  assert.match(body, new RegExp(`"mm":${midpoint.replace(/[.]/g, "\\.")}`));
});

test("配置请求把导入 JSON 中的原始小数保留为字符串内容", () => {
  const midpoint = "1.0000000596046447753906250000000000000000000001";
  const record = `{"config":{"gain":${midpoint}}}`;
  const body = stringifyJson({ action:"config_validate", record });
  assert.equal(JSON.parse(body).record, record);
  assert.match(body, new RegExp(midpoint.replace(/[.]/g, "\\.")));
});

const original = '{\n  "format_version": 1,\n  "config": {"future_u64": 18446744073709551615, "label": "A\\nB"}\n}\n';

test("unrelated form replacements preserve large integer and raw formatting", () => {
  const changed = replaceLiteral(original, "/format_version", "2");
  assert.match(changed, /18446744073709551615/);
  assert.match(changed, /"label": "A\\nB"/);
  assert.equal(literalAt(changed, "/config/future_u64"), "18446744073709551615");
});

test("diff exposes literals without converting them to JavaScript numbers", () => {
  const changed = replaceLiteral(original, "/config/label", '"C"');
  assert.deepEqual(diffPointers(original, changed), [{ pointer:"/config/label", before:'"A\\nB"', after:'"C"' }]);
});

test("span parser rejects syntactically invalid JSON literals", () => {
  assert.throws(() => parseJsonTree('{"config": invalid}'));
  assert.throws(() => parseJsonTree('{"config": 1} trailing'));
});
