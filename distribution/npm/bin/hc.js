#!/usr/bin/env node
// Runs the cached herdr-core binary under its short CLI alias (hc).
import { runCLI } from "./herdr-core.js";

runCLI("hc").catch((err) => {
  process.stderr.write(`hc: ${err.message}\n`);
  process.exit(1);
});
