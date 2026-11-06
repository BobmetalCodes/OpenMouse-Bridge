import assert from "node:assert/strict";
import test from "node:test";

import { inControlOrder, usesMouseCollectionForControl } from "./hid-device-adapter.mjs";

const RAZER = 0x1532;
const LOGITECH = 0x046d;
const mouse = (vendorId) => ({ vendorId, usagePage: 1, usage: 2 });

test("a Razer mouse collection stays a candidate on Linux", () => {
  assert.equal(usesMouseCollectionForControl(mouse(RAZER), "linux"), true);
});

test("an interface that is only a mouse is tried last", () => {
  const pointer = [mouse(RAZER)];
  const mixed = [mouse(RAZER), { vendorId: RAZER, usagePage: 0x0c, usage: 1 }];
  const vendor = [{ vendorId: RAZER, usagePage: 0x59, usage: 1 }];
  assert.deepEqual(inControlOrder([pointer, mixed, vendor], "linux"), [mixed, vendor, pointer]);
});

test("macOS and Windows keep skipping it", () => {
  assert.equal(usesMouseCollectionForControl(mouse(RAZER), "darwin"), false);
  assert.equal(usesMouseCollectionForControl(mouse(RAZER), "win32"), false);
});

test("other vendors keep skipping it", () => {
  assert.equal(usesMouseCollectionForControl(mouse(LOGITECH), "linux"), false);
});

test("collections other than the mouse are not affected", () => {
  const keyboard = { vendorId: RAZER, usagePage: 1, usage: 6 };
  const consumer = { vendorId: RAZER, usagePage: 0x0c, usage: 1 };
  assert.equal(usesMouseCollectionForControl(keyboard, "linux"), false);
  assert.equal(usesMouseCollectionForControl(consumer, "linux"), false);
});
