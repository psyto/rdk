# Princeps repo bootstrap

This directory (`/Users/hiroyusai/src/princeps/`) currently contains the scaffold content for the initial `psyto/princeps` repo:

- `README.md` — top-level README (Princeps positioning + roadmap + status)
- `LICENSE` — Apache 2.0 (canonical text from apache.org)
- `docs/adr/README.md` — ADR index
- `docs/adr/001-007.md` — 7 architectural decisions (locked 2026-05-31)
- `SCAFFOLD.md` — this file (will NOT be committed to repo)

**This same directory will become the live `psyto/princeps` working tree** once you run the playbook below. The workflow:

1. Backup scaffold files to `/tmp`
2. Copy openhl's source files (no `.git`) into this dir — openhl repo itself stays completely untouched
3. Restore scaffold files on top (README, LICENSE, ADRs overwrite openhl's equivalents where present)
4. Rebrand `bin/openhl` → `bin/princeps`
5. Init fresh git, commit, push to new `psyto/princeps`

**Key design choice (2026-05-31)**: Princeps is a **fresh fork without shared git history**. openhl at `github.com/psyto/openhl` continues unchanged as the open-source HL reference implementation (rethlab citations against openhl SHAs keep working forever). Princeps diverges from here as its own production codebase.

## Playbook to bootstrap psyto/princeps

### Step 1: Backup scaffold files

```bash
cp -r /Users/hiroyusai/src/princeps /tmp/princeps-scaffold-backup
```

### Step 2: Copy openhl source files into this dir (excluding .git)

```bash
rsync -av --exclude='.git' --exclude='target' /Users/hiroyusai/src/openhl/ /Users/hiroyusai/src/princeps/
```

This brings all openhl source code into the princeps dir as plain files — no git history, no remote pointers. The openhl repo at `github.com/psyto/openhl` is untouched; your local `/Users/hiroyusai/src/openhl/` working tree is also untouched (rsync only reads from it).

Verify:

```bash
cd /Users/hiroyusai/src/princeps
ls bin/openhl                  # should exist (openhl source)
ls crates/                     # should show all openhl crates
ls Cargo.toml Cargo.lock       # should exist
ls -d .git 2>/dev/null         # should NOT exist (no git history yet)
```

### Step 3: Restore scaffold files on top (overwriting openhl's where they collide)

```bash
cp /tmp/princeps-scaffold-backup/README.md ./README.md       # overwrites openhl's README
cp /tmp/princeps-scaffold-backup/LICENSE ./LICENSE           # overwrites openhl's LICENSE if any
mkdir -p docs/adr
cp -r /tmp/princeps-scaffold-backup/docs/adr/* ./docs/adr/
cp /tmp/princeps-scaffold-backup/SCAFFOLD.md ./SCAFFOLD.md   # bootstrap doc, will be gitignored
```

### Step 4: Rebrand `bin/openhl` → `bin/princeps`

```bash
mv bin/openhl bin/princeps
```

(Plain `mv`, not `git mv`, because we haven't `git init`'d yet.)

Then manually edit:

- **Top-level `Cargo.toml`** — change workspace member `"bin/openhl"` to `"bin/princeps"`
- **`bin/princeps/Cargo.toml`** — change `name = "openhl"` to `name = "princeps"`

### Step 5: Verify build still works

```bash
cargo build --release
cargo test --workspace
```

If tests fail, the rename broke something — fix before continuing.

### Step 6: Initialize git fresh

```bash
git init
git branch -m main
```

### Step 7: Exclude SCAFFOLD.md from the commit

```bash
# Append to existing .gitignore (inherited from openhl):
echo "" >> .gitignore
echo "# Bootstrap meta-doc — not part of the repo" >> .gitignore
echo "SCAFFOLD.md" >> .gitignore
```

### Step 8: Initial commit

```bash
git add -A
git status | grep SCAFFOLD.md    # should show "ignored" or nothing
git commit -m "feat: initial Princeps commit — derived from openhl reference impl, Apache 2.0, 7 ADRs locked"
```

### Step 9: Create the GitHub repo and push

```bash
gh repo create psyto/princeps --public \
  --description "The DeFi prime broker L1 on Reth. Lending → options → structured products → institutional rails." \
  --source=. --remote=origin --push
```

### Step 10: Add topics

```bash
gh repo edit psyto/princeps \
  --add-topic defi \
  --add-topic prime-broker \
  --add-topic reth \
  --add-topic ethereum \
  --add-topic l1 \
  --add-topic rust \
  --add-topic lending \
  --add-topic options \
  --add-topic structured-products
```

Then pin the repo on your psyto profile (via GitHub UI).

### Step 11: Cleanup

```bash
rm -rf /tmp/princeps-scaffold-backup
```

## After this is done

- **psyto/openhl** is unchanged — continues as the open-source HL reference implementation, rethlab citations keep working
- **psyto/princeps** is live at `github.com/psyto/princeps` — fresh git history starting with a single "initial Princeps commit" containing the full openhl codebase + Princeps scaffold + rebrand
- `/Users/hiroyusai/src/princeps/` is the live working tree for psyto/princeps
- README is the announcement landing page
- ADR doc backs up the "7 locked architectural decisions" claim in the announcement
- Crate internal names (`openhl-types` → `princeps-types` in individual Cargo.toml files) can be renamed in follow-up PRs — not blocking initial commit
- Future openhl improvements (e.g. Stage 13l libp2p) will need to be manually ported into princeps if you want them — they will NOT auto-propagate, by design
- Ready to publish the X thread + long-form announcement
