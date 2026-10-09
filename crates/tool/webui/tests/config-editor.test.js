// Copyright The eha-sdk Contributors
import test from "node:test";
import assert from "node:assert/strict";
import { diffPointers, literalAt, replaceLiteral } from "../static/config-editor.js";

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
