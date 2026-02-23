#!/bin/bash
set -euo pipefail

# nockchain-jammer installer
# Run with: curl -fsSL https://raw.githubusercontent.com/nocktoshi/nockchain-jammer/main/install.sh | bash

# Load configuration from .env file if it exists
if [[ -f .env ]]; then
    set -a
    source .env
    set +a
fi

# Defaults (override in .env)
REPO_URL="${REPO_URL:-https://github.com/nocktoshi/nockchain-jammer}"
INSTALL_DIR="${INSTALL_DIR:-/opt/nockchain-jammer}"
SERVICE_NAME="${SERVICE_NAME:-nockchain-jammer-api}"
JAMS_DIR="${JAMS_DIR:-/usr/share/nginx/html/jams}"
API_BINARY_PATH="${API_BINARY_PATH:-/usr/local/bin/nockchain-jammer-api}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()    { echo -e "${BLUE}[INFO]${NC} $1"; }
log_success() { echo -e "${GREEN}[OK]${NC} $1"; }
log_warn()    { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error()   { echo -e "${RED}[ERROR]${NC} $1"; }

if [[ $EUID -ne 0 ]]; then
    log_error "This script must be run as root (use sudo)"
    exit 1
fi

if [[ ! -f /etc/os-release ]] || ! grep -qi "ubuntu\|debian" /etc/os-release; then
    log_error "This installer targets Ubuntu/Debian. See README for manual steps."
    exit 1
fi

log_info "Starting nockchain-jammer installation..."

# ── Dependencies ──────────────────────────────────────────────────────
log_info "Installing build dependencies..."
apt-get update -qq
apt-get install -y -qq curl git build-essential pkg-config libssl-dev protobuf-compiler >/dev/null

# ── Jammer user ───────────────────────────────────────────────────────
JAMMER_HOME="/var/lib/jammer"
if ! id jammer >/dev/null 2>&1; then
    useradd --system --shell /bin/bash --home "$JAMMER_HOME" --create-home jammer
fi
mkdir -p "$JAMMER_HOME"
chown jammer:jammer "$JAMMER_HOME"

# ── Rust toolchain (for jammer user) ─────────────────────────────────
JAMMER_CARGO="$JAMMER_HOME/.cargo/bin/cargo"
if [[ ! -x "$JAMMER_CARGO" ]]; then
    log_info "Installing Rust toolchain for jammer user..."
    sudo -u jammer \
        RUSTUP_HOME="$JAMMER_HOME/.rustup" \
        CARGO_HOME="$JAMMER_HOME/.cargo" \
        bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'
fi

if [[ -x "$JAMMER_HOME/.cargo/bin/rustup" ]]; then
    log_info "Installing Rust nightly (required by nockchain dependency)..."
    sudo -u jammer \
        RUSTUP_HOME="$JAMMER_HOME/.rustup" \
        CARGO_HOME="$JAMMER_HOME/.cargo" \
        "$JAMMER_HOME/.cargo/bin/rustup" install nightly
    log_info "Setting default Rust toolchain to nightly..."
    sudo -u jammer \
        RUSTUP_HOME="$JAMMER_HOME/.rustup" \
        CARGO_HOME="$JAMMER_HOME/.cargo" \
        "$JAMMER_HOME/.cargo/bin/rustup" default nightly
fi

if [[ ! -x "$JAMMER_CARGO" ]]; then
    log_error "Cargo not found at $JAMMER_CARGO"
    exit 1
fi
log_info "Using cargo: $("$JAMMER_CARGO" --version 2>/dev/null || echo unknown)"

# ── Clone / update repo ──────────────────────────────────────────────
git config --global --add safe.directory "$INSTALL_DIR"
if [[ -d "$INSTALL_DIR/.git" ]]; then
    log_warn "Installation directory exists, updating..."
    git -C "$INSTALL_DIR" pull
else
    log_info "Cloning repository..."
    git clone "$REPO_URL" "$INSTALL_DIR"
fi

# ── Build ─────────────────────────────────────────────────────────────
log_info "Building API binary..."
chown -R jammer:jammer "$INSTALL_DIR"
(cd "$INSTALL_DIR" && sudo -u jammer \
    RUSTUP_HOME="$JAMMER_HOME/.rustup" \
    CARGO_HOME="$JAMMER_HOME/.cargo" \
    "$JAMMER_CARGO" +nightly build --release --manifest-path "$INSTALL_DIR/api/Cargo.toml")

# ── Install ───────────────────────────────────────────────────────────
log_info "Installing files..."

if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    log_info "Stopping existing service..."
    systemctl stop "$SERVICE_NAME"
fi

cp "$INSTALL_DIR/api/target/release/nockchain-jammer-api" "$API_BINARY_PATH"
chmod +x "$API_BINARY_PATH"

# Website + jam files directory
mkdir -p "$JAMS_DIR"
cp "$INSTALL_DIR/website/index.html" "$JAMS_DIR/"
cp "$INSTALL_DIR/website/style.css" "$JAMS_DIR/"
cp "$INSTALL_DIR/website/jam-icon.png" "$JAMS_DIR/" 2>/dev/null || true
cp "$INSTALL_DIR/website/BerkeleyMono-Regular.ttf" "$JAMS_DIR/" 2>/dev/null || true
chown -R jammer:jammer "$JAMS_DIR"

# ── Environment file ─────────────────────────────────────────────────
if [[ ! -f /etc/nockchain-jammer.env ]]; then
    log_info "Creating runtime configuration..."
    if [[ -f "$INSTALL_DIR/.env" ]]; then
        cp "$INSTALL_DIR/.env" /etc/nockchain-jammer.env
    else
        cp "$INSTALL_DIR/.env.example" /etc/nockchain-jammer.env
    fi
fi

# Generate API key if missing
if ! grep -q "^API_KEY=.\+" /etc/nockchain-jammer.env 2>/dev/null; then
    API_KEY=$(openssl rand -hex 32)
    log_info "Generated API key: ${API_KEY:0:8}..."
    if grep -q "^API_KEY=" /etc/nockchain-jammer.env; then
        sed -i "s|^API_KEY=.*|API_KEY=$API_KEY|" /etc/nockchain-jammer.env
    else
        echo "API_KEY=$API_KEY" >> /etc/nockchain-jammer.env
    fi
else
    API_KEY=$(grep "^API_KEY=" /etc/nockchain-jammer.env | cut -d'=' -f2)
    log_info "Using existing API key: ${API_KEY:0:8}..."
fi

# ── Systemd service ──────────────────────────────────────────────────
log_info "Creating systemd service..."
cat > /etc/systemd/system/${SERVICE_NAME}.service << EOF
[Unit]
Description=Nockchain Jammer API
After=network.target

[Service]
Type=simple
User=root
EnvironmentFile=/etc/nockchain-jammer.env
ExecStart=$API_BINARY_PATH
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

# ── Sudo for nockchain export ────────────────────────────────────────
NC_USER=""
if grep -q "^NOCKCHAIN_USER=" /etc/nockchain-jammer.env 2>/dev/null; then
    NC_USER=$(grep "^NOCKCHAIN_USER=" /etc/nockchain-jammer.env | cut -d'=' -f2)
fi

SYSTEMCTL_PATH="$(command -v systemctl)"
cat > /etc/sudoers.d/nockchain-jammer << SUDOEOF
root ALL=(ALL) NOPASSWD: $SYSTEMCTL_PATH stop nockchain, $SYSTEMCTL_PATH start nockchain, $SYSTEMCTL_PATH is-active nockchain, $SYSTEMCTL_PATH is-active --quiet nockchain
SUDOEOF

if [[ -n "$NC_USER" ]]; then
    echo "root ALL=($NC_USER) NOPASSWD: ALL" >> /etc/sudoers.d/nockchain-jammer
fi
chmod 440 /etc/sudoers.d/nockchain-jammer

# ── Start ─────────────────────────────────────────────────────────────
log_info "Starting service..."
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"
systemctl start "$SERVICE_NAME"

sleep 2
API_PORT=$(grep "^API_PORT=" /etc/nockchain-jammer.env 2>/dev/null | cut -d'=' -f2)
API_PORT="${API_PORT:-80}"

if curl -sf "http://localhost:$API_PORT/api/status" >/dev/null 2>&1; then
    log_success "API is responding on port $API_PORT"
else
    log_warn "API not yet responding. Check: systemctl status $SERVICE_NAME"
fi

log_success "Installation complete!"
echo
echo "========================================"
echo "  Nockchain Jammer"
echo "========================================"
echo "API Key:  $API_KEY"
echo "Port:     $API_PORT"
echo "Jams:     $JAMS_DIR"
echo "Service:  $SERVICE_NAME"
echo
echo "Test:     curl http://localhost:$API_PORT/api/status"
echo "Make jam: curl -X POST -H 'X-API-Key: $API_KEY' http://localhost:$API_PORT/api/make-jam"
echo "Website:  http://your-server:$API_PORT/jams/"
echo "========================================"
