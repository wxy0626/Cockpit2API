import assert from "node:assert/strict";
import test from "node:test";
import { parseTimeToMinutes } from "./workbuddyAutoCheckinService.ts";

test("parseTimeToMinutes correctly converts HH:mm to minutes", () => {
  assert.equal(parseTimeToMinutes("06:00"), 360);
  assert.equal(parseTimeToMinutes("12:30"), 750);
  assert.equal(parseTimeToMinutes("00:00"), 0);
  assert.equal(parseTimeToMinutes("23:59"), 1439);
});
