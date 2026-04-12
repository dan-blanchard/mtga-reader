// Smoke test for Mono scanners against Wine Arena.
// Usage: sudo node try_wine_all.js
const r = require("./index.js");

console.log("=== readMtgaCardsMono ===");
const t0 = Date.now();
const cards = r.readMtgaCardsMono("MTGA.exe");
console.log(`Time: ${Date.now() - t0}ms`);
if (cards.error) {
  console.error("ERROR:", cards.error);
} else {
  console.log(`Cards: ${cards.cards.length} unique`);
  console.log("First 3:", JSON.stringify(cards.cards.slice(0, 3)));
}

console.log("\n=== readMtgaCardDatabaseMono ===");
const t1 = Date.now();
const db = r.readMtgaCardDatabaseMono("MTGA.exe");
console.log(`Time: ${Date.now() - t1}ms`);
if (db.error) {
  console.error("ERROR:", db.error);
} else {
  const withSet = db.cards.filter((c) => c.set && c.collectorNumber);
  console.log(`Cards: ${db.cards.length} total, ${withSet.length} with set+num`);
  console.log("First 3:", JSON.stringify(db.cards.slice(0, 3)));
}

console.log("\n=== readMtgaInventoryMono ===");
const t2 = Date.now();
const inv = r.readMtgaInventoryMono("MTGA.exe");
console.log(`Time: ${Date.now() - t2}ms`);
if (inv.error) {
  console.error("ERROR:", inv.error);
} else {
  console.log(JSON.stringify(inv, null, 2));
}

// Cross-check: compare Mono inventory against IL2CPP inventory
console.log("\n=== Cross-check: IL2CPP vs Mono inventory ===");
const il2cppInv = r.readMtgaInventory("MTGA");
if (!il2cppInv.error && !inv.error) {
  const match = inv.wcCommon === il2cppInv.wcCommon
    && inv.wcUncommon === il2cppInv.wcUncommon
    && inv.wcRare === il2cppInv.wcRare
    && inv.wcMythic === il2cppInv.wcMythic
    && inv.gold === il2cppInv.gold
    && inv.gems === il2cppInv.gems;
  console.log(match ? "MATCH - same account data" : "MISMATCH");
  if (!match) {
    console.log("  IL2CPP:", JSON.stringify(il2cppInv));
    console.log("  Mono:  ", JSON.stringify(inv));
  }
}
