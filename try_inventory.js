// Smoke-test for readMtgaInventory. Requires sudo (task_for_pid).
//
// Usage:
//   sudo node try_inventory.js
const r = require("./index.js");

const t0 = Date.now();
const result = r.readMtgaInventory("MTGA");
const ms = Date.now() - t0;

if (result.error) {
  console.error("ERROR:", result.error);
  process.exit(1);
}

console.log(`Read inventory in ${ms}ms`);
console.log(JSON.stringify(result, null, 2));

// Quick sanity: also re-run the collection and count total cards so
// we can confirm we're reading from the same logged-in player.
const coll = r.readMtgaCards("MTGA");
if (!coll.error && coll.cards) {
  const unique = coll.cards.length;
  const total = coll.cards.reduce((a, c) => a + c.quantity, 0);
  console.log(`Collection: ${unique} unique printings, ${total} total copies`);
}
