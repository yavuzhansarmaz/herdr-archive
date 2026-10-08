# Cross-compat fixtures

`python-*.json` are real `record.json` files written by the **Python** shelf's
own `capture()` (from `/home/yohansin/Desktop/PP/herdr-shelf`), and
`python-*.tree.json` are that same checkout's `restore.build_tree()` output
for each. The Rust `cross_compat` test loads each record and asserts its own
`build_tree` output equals the `.tree.json` file.

Regenerate from the Python checkout:

```sh
python3 tests/fixtures/gen_fixtures.py tests/fixtures/
```

(Regeneration mints fresh random archive-id suffixes; record contents are
otherwise stable. Record ids differ from run to run by design.)
