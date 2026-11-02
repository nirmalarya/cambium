# Getting started

Run the reference setup (PostgreSQL + RustFS object storage + `cmbd`) with Docker on your
machine, then make your first changes. About 15 minutes.

## 1. Start the server
Needs Docker and Rust (for the client).
```sh
git clone https://github.com/nirmalarya/cambium && cd cambium
# secrets for the database and object store, made up on the spot (kept in .env, git-ignored)
printf 'POSTGRES_PASSWORD=%s\nRUSTFS_ACCESS_KEY=%s\nRUSTFS_SECRET_KEY=%s\n' \
  "$(openssl rand -hex 24)" "cambium$(openssl rand -hex 4)" "$(openssl rand -hex 24)" > .env
docker compose up --build -d          # first build takes a few minutes
docker compose ps                     # postgres, rustfs and cmbd should be running
docker compose exec cmbd cmbd token alice    # prints a token: cmbt_…  (keep it secret)
```
The server listens on `http://127.0.0.1:7878` (this machine only).

## 2. Install the client
```sh
cargo build --release -p cambium-cli
export PATH="$PWD/target/release:$PATH"
export CAMBIUM_SERVER=http://127.0.0.1:7878
export CAMBIUM_TOKEN=cmbt_…           # the token from step 1
cmb whoami                            # alice
```

## 3. Your first changes
```sh
cd /tmp && cmb init demo && cd demo
echo "# Demo" > README.md
mkdir src && echo 'fn main() {}' > src/main.rs
cmb add README.md src
cmb commit -m "first files"           # lands on main
cmb log
```
Make a change and look at it before committing:
```sh
echo 'fn helper() {}' >> src/main.rs
cmb status
cmb diff
cmb add src && cmb commit -m "add helper"
cmb blame src/main.rs
```

## 4. Open the web UI
Go to <http://127.0.0.1:7878/ui> and paste your token. Open **demo**: you can browse files,
read the README, see history and blame, search, and open reviews and settings.

## 5. Try a review
Reviews need a second person; mint a token for bob in another terminal:
```sh
docker compose exec cmbd cmbd token bob
```
As alice, protect `main` and propose a change as a draft:
```sh
cmb protect main 1                    # main now changes only through approved reviews
echo "More docs." >> README.md && cmb add README.md
cmb commit --draft -m "expand README"
cmb review create -m "Expand the README"
```
As bob (`CAMBIUM_TOKEN=<bob's token>`), approve it — in the web UI or with
`cmb review approve <n>` — then as alice: `cmb review merge <n>`. The change lands on `main`.

## Stop or reset
```sh
docker compose down        # stop, keep data
docker compose down -v     # stop and delete all data
```
Next: [everyday workflows](workflows.md).
