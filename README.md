# nockchain-jammer

<img width="632" height="395" alt="image-removebg-preview (1)" src="https://github.com/user-attachments/assets/7a806595-0ad4-49fd-b696-aa4f8f047790" />

Make yummy jams. Serves Nockchain state jam binaries with SHA-256 checksum verification, and provides an API to trigger new jam builds from the website.

## Components

| File | Purpose |
|------|---------|
| `jams.html` | Frontend — lists jam files, checksums, and admin trigger UI |
| `make-jam.sh` | Stops the nockchain service, hashes `.jam` files, writes `SHA256SUMS`, restarts service |
| `api/` | Axum (Rust) API server that runs `make-jam.sh` on demand |

## API Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `POST` | `/api/make-jam` | `X-API-Key` header | Run `make-jam.sh hash` and return output |
| `GET`  | `/api/status` | none | Check if a job is currently running |

## Deployment

### 1. Build the API binary

```bash
cd api
cargo build --release
```

The binary is at `api/target/release/nockchain-jammer-api`.

### 2. Install on the server

```bash
# Copy binary
scp api/target/release/nockchain-jammer-api server:/usr/local/bin/

# Copy the script
scp make-jam.sh server:/usr/local/bin/
chmod +x /usr/local/bin/make-jam.sh

# Copy the HTML
scp jams.html server:/usr/share/nginx/html/jams/index.html
```

### 3. Set up the systemd service

```bash
# Copy the unit file
sudo cp api/nockchain-jammer-api.service /etc/systemd/system/

# Edit it to set your real API key
sudo systemctl edit --full nockchain-jammer-api
# Change API_KEY=CHANGE_ME to your secret key

# Enable and start
sudo systemctl daemon-reload
sudo systemctl enable --now nockchain-jammer-api

# Verify
systemctl status nockchain-jammer-api
```

### 4. Configure nginx

Add the reverse proxy block to your existing nginx server config (e.g. `/etc/nginx/sites-available/default`). The snippet is in `api/nginx-api.conf`:

```nginx
location /api/ {
    proxy_pass         http://127.0.0.1:3001;
    proxy_http_version 1.1;
    proxy_set_header   Host              $host;
    proxy_set_header   X-Real-IP         $remote_addr;
    proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header   X-Forwarded-Proto $scheme;
    proxy_read_timeout 120s;
    proxy_send_timeout 120s;
}
```

Then test and reload:

```bash
sudo nginx -t && sudo systemctl reload nginx
```

### 5. Permissions

`make-jam.sh` calls `systemctl stop/start nockchain`. The API service user needs permission to do this. Either:

- Run the API as root (simple but less secure), or
- Add a sudoers rule for the specific commands:

```bash
# /etc/sudoers.d/nockchain-jammer
jammer ALL=(ALL) NOPASSWD: /bin/systemctl stop nockchain, /bin/systemctl start nockchain, /bin/systemctl is-active nockchain
```

Then update `make-jam.sh` to use `sudo systemctl` instead of bare `systemctl`, or run the service as root.

## Environment Variables (API)

| Variable | Default | Description |
|----------|---------|-------------|
| `API_KEY` | *(empty — prints warning)* | Shared secret for `X-API-Key` header |
| `SCRIPT_PATH` | `/usr/local/bin/make-jam.sh` | Absolute path to the jam script |
