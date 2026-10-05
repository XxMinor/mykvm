import assert from "node:assert/strict";
import { test } from "node:test";
import { compareVersions } from "./versions.ts";

test("versions order by semver precedence", () => {
  const ordered = [
    "0.9.12",
    "0.9.13-beta.99",
    "0.9.13-beta.99.9",
    "0.9.13-beta.99.10",
    "0.9.13-beta.100",
    "0.9.13-beta.100.1",
    "0.9.13",
    "0.10.0",
  ];
  for (let i = 0; i + 1 < ordered.length; i += 1) {
    assert.equal(compareVersions(ordered[i], ordered[i + 1]), -1, `${ordered[i]} < ${ordered[i + 1]}`);
    assert.equal(compareVersions(ordered[i + 1], ordered[i]), 1, `${ordered[i + 1]} > ${ordered[i]}`);
  }
  assert.equal(compareVersions("0.9.13-beta.100", "0.9.13-beta.100"), 0);
});
