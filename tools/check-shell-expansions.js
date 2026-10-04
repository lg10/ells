const fs = require('fs');
const files = process.argv.slice(2);
let bad = 0;
for (const f of files) {
  const text = fs.readFileSync(f, 'utf8');
  // $VAR (no braces) immediately followed by a non-ASCII char: bash 3.2 in a
  // non-UTF-8 locale folds those bytes into the parameter name.
  const re = /\$[A-Za-z_][A-Za-z0-9_]*[^\x00-\x7F]/g;
  let m;
  while ((m = re.exec(text))) {
    const line = text.slice(0, m.index).split('\n').length;
    console.log(`${f}:${line}: ${JSON.stringify(m[0])}`);
    bad++;
  }
}
if (bad) { console.error(`FAIL: ${bad} expansion(s) adjacent to non-ASCII characters`); process.exit(1); }
console.log('OK: no $VAR immediately followed by a non-ASCII character');
