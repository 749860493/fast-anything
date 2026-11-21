// fast-anything call recorder, preloaded with "node --require". Not meant to be used by hand.
"use strict";
const fs = require("node:fs");

const out = process.env.FAST_ANYTHING_RECORD_OUT;
const meta = process.env.FAST_ANYTHING_RECORD_META;
const limit = Number(process.env.FAST_ANYTHING_RECORD_LIMIT || 500);
const kept = [];
const unsupported = new Set();
let calls = 0;
let skipped = 0;
let seed = 0xfa57;
const rand = (n) => {
  seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
  return seed % n;
};

function enc(v) {
  if (v === null || typeof v === "string" || typeof v === "boolean") return v;
  if (typeof v === "number") return Number.isFinite(v) ? (Object.is(v, -0) ? { $float: "-0" } : v) : { $float: String(v) };
  if (v === undefined) return { $undefined: 0 };
  if (typeof v === "bigint") return { $bigint: v.toString() };
  if (Array.isArray(v)) return v.map(enc);
  if (ArrayBuffer.isView(v) && !(v instanceof DataView))
    return { $typed: [v.constructor.name, Array.from(v, (x) => (typeof x === "bigint" ? x.toString() : x))] };
  if (v instanceof Map) return { $map: [...v].map(([k, x]) => [enc(k), enc(x)]) };
  if (v instanceof Set) return { $set: [...v].map(enc) };
  if (typeof v === "object") {
    const proto = Object.getPrototypeOf(v);
    if (proto === Object.prototype || proto === null) return { $object: Object.entries(v).map(([k, x]) => [k, enc(x)]) };
  }
  throw new Error(typeof v === "object" ? (v.constructor?.name ?? "object") : typeof v);
}

function store(slot, c) {
  if (kept.length < limit) kept.push(c);
  else kept[slot % limit] = c;
}

globalThis.__fastAnythingWrap = (fn) =>
  function (...args) {
    calls++;
    const slot = kept.length < limit ? kept.length : rand(calls);
    let c = null;
    if (slot < limit) {
      try {
        c = { args: args.map(enc) };
      } catch (e) {
        skipped++;
        unsupported.add(e.message);
      }
    }
    let result;
    try {
      result = fn.apply(this, args);
    } catch (e) {
      if (c) {
        c.raises = (e && e.name) || "Error";
        store(slot, c);
      }
      throw e;
    }
    if (c) {
      try {
        c.result = enc(result);
        store(slot, c);
      } catch (e) {
        skipped++;
        unsupported.add(e.message);
      }
    }
    return result;
  };

process.on("exit", () => {
  fs.writeFileSync(out, kept.map((c) => JSON.stringify(c)).join("\n") + (kept.length ? "\n" : ""));
  fs.writeFileSync(meta, JSON.stringify({ calls, kept: kept.length, skipped, unsupported: [...unsupported] }));
});
