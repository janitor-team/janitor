// Tests for the run log viewer; run with `make test-js`.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {
  LINES_PER_BLOCK,
  lineSplitter,
  findNext,
  lineUrl,
  lineFromHash,
} = require("../py/janitor/site/_static/log.js");

function split(size, chunks) {
  const blocks = [];
  const splitter = lineSplitter(size, (lines) => blocks.push(lines));
  chunks.forEach((chunk) => splitter.push(chunk));
  splitter.end();
  return blocks;
}

test("lineSplitter", () => {
  assert.deepEqual(split(2, ["a\nb\nc\n"]), [["a", "b"], ["c"]]);
  assert.deepEqual(split(2, ["a\nb", "\nc"]), [["a", "b"], ["c"]]);
  assert.deepEqual(split(3, ["fir", "st\n\nth", "ird\n"]), [["first", "", "third"]]);
  assert.deepEqual(split(2, ["a\r\n", "b"]), [["a\r", "b"]]);
  assert.deepEqual(split(2, [""]), []);
});

function blocks(lines) {
  const texts = [];
  for (let i = 0; i < lines.length; i += LINES_PER_BLOCK) {
    texts.push(lines.slice(i, i + LINES_PER_BLOCK).join("\n"));
  }
  return texts;
}

test("findNext", () => {
  const lines = Array.from({ length: 1200 }, (_, i) => `line ${i + 1}`);
  lines[2] = "Error: first";
  lines[700] = "error: second";
  const texts = blocks(lines);
  assert.deepEqual(
    [
      findNext(texts, "error", 0),
      findNext(texts, "error", 3),
      findNext(texts, "error", 701),
      findNext(texts, "line 1200", 0),
      findNext(texts, "missing", 0),
      findNext([], "error", 0),
    ],
    [3, 701, 3, 1200, null, null],
  );
});

test("lineUrl", () => {
  assert.deepEqual(
    [1, 1000, 1001, 2500].map((n) => lineUrl(n, 1000)),
    ["?start=1#L1", "?start=1#L1000", "?start=1001#L1001", "?start=2001#L2500"],
  );
});

test("lineFromHash", () => {
  assert.deepEqual(
    ["#L1", "#L1234", "#L0", "#L01", "#Lx", "#log-end", ""].map(lineFromHash),
    [1, 1234, null, null, null, null, null],
  );
});
