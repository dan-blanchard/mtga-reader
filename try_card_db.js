// Smoke-test for readMtgaCardDatabase. Requires sudo (task_for_pid).
//
// Usage:
//   sudo node try_card_db.js
const r = require("./index.js");

const t0 = Date.now();
const result = r.readMtgaCardDatabase("MTGA");
const ms = Date.now() - t0;

if (result.error) {
  console.error("ERROR:", result.error);
  process.exit(1);
}

const cards = result.cards || [];
console.log(`Read ${cards.length} cards in ${ms}ms`);
console.log("First 5:", JSON.stringify(cards.slice(0, 5), null, 2));

const withSet = cards.filter((c) => c.set && c.collectorNumber);
console.log(`${withSet.length}/${cards.length} have both set + collectorNumber`);

// Top-10 sets to sanity-check the distribution
const setCounts = {};
for (const c of cards) {
  if (c.set) setCounts[c.set] = (setCounts[c.set] || 0) + 1;
}
const topSets = Object.entries(setCounts)
  .sort((a, b) => b[1] - a[1])
  .slice(0, 10);
console.log("Top sets:", topSets);

// Cross-check against readMtgaCards: pick 3 collection cards and see
// if their grpIds resolve.
const coll = r.readMtgaCards("MTGA");
if (!coll.error && coll.cards && coll.cards.length) {
  const byGrpId = new Map(cards.map((c) => [c.grpId, c]));
  const samples = coll.cards.slice(0, 5).map((c) => ({
    grpId: c.cardId,
    quantity: c.quantity,
    resolved: byGrpId.get(c.cardId),
  }));
  console.log("Collection cross-check (first 5):", JSON.stringify(samples, null, 2));
}
