#!/bin/bash
set -euo pipefail

# nockchain-jammer installer
# Run with: curl -fsSL https://raw.githubusercontent.com/nocktoshi/nockchain-jammer/main/install.sh | bash

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

REPO_URL="${REPO_URL:-https://github.com/nocktoshi/nockchain-jammer}"
INSTALL_DIR="${INSTALL_DIR:-/opt/nockchain-jammer}"
SERVICE_NAME="${SERVICE_NAME:-nockchain-jammer-api}"
JAMS_DIR="${JAMS_DIR:-/usr/share/nginx/html/jams}"
API_BINARY_PATH="${API_BINARY_PATH:-/usr/local/bin/nockchain-jammer-api}"
# User that runs `cargo` (not root). Defaults to sudo invoker; set in .env if needed.
BUILD_USER="${BUILD_USER:-${SUDO_USER:-}}"

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

if [[ -z "$BUILD_USER" ]] || [[ "$BUILD_USER" == "root" ]]; then
    log_error "Set BUILD_USER in .env (or run: sudo -u youruser bash install.sh)."
    log_error "Install does not use root's cargo; Rust should stay on your normal user account."
    exit 1
fi

if ! id "$BUILD_USER" >/dev/null 2>&1; then
    log_error "BUILD_USER '$BUILD_USER' does not exist on this system."
    exit 1
fi

if ! sudo -u "$BUILD_USER" -H bash -lc 'command -v cargo >/dev/null 2>&1'; then
    log_error "cargo not found for user '$BUILD_USER'. Install Rust nightly with rustup (see README)."
    exit 1
fi

log_info "Starting nockchain-jammer installation (build as user: $BUILD_USER)..."

log_info "Installing build dependencies..."
apt-get update -qq
apt-get install -y -qq curl git build-essential pkg-config libssl-dev protobuf-compiler >/dev/null

if [[ -d "$INSTALL_DIR/.git" ]]; then
    log_warn "Installation directory exists, updating..."
    git -C "$INSTALL_DIR" pull
else
    log_info "Cloning repository..."
    git clone "$REPO_URL" "$INSTALL_DIR"
fi

chown -R "$BUILD_USER:$BUILD_USER" "$INSTALL_DIR"
sudo -u "$BUILD_USER" -H git config --global --add safe.directory "$INSTALL_DIR"

log_info "Building API binary as $BUILD_USER (rust-toolchain.toml selects nightly)..."
[[ -f "$INSTALL_DIR/sync_to_gdrive.sh" ]] && chmod +x "$INSTALL_DIR/sync_to_gdrive.sh"
sudo -u "$BUILD_USER" -H bash -lc "cd '$INSTALL_DIR' && cargo build --release --manifest-path api/Cargo.toml"

if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    log_info "Stopping existing service..."
    systemctl stop "$SERVICE_NAME"
fi

log_info "Installing files..."
cp "$INSTALL_DIR/api/target/release/nockchain-jammer-api" "$API_BINARY_PATH"
chmod +x "$API_BINARY_PATH"

mkdir -p "$JAMS_DIR"
cp "$INSTALL_DIR/website/index.html" "$JAMS_DIR/"
cp "$INSTALL_DIR/website/style.css" "$JAMS_DIR/"
cp "$INSTALL_DIR/website/jam-icon.png" "$JAMS_DIR/" 2>/dev/null || true
cp "$INSTALL_DIR/website/BerkeleyMono-Regular.ttf" "$JAMS_DIR/" 2>/dev/null || true

if [[ ! -f /etc/nockchain-jammer.env ]]; then
    log_info "Creating runtime configuration..."
    if [[ -f "$INSTALL_DIR/.env" ]]; then
        cp "$INSTALL_DIR/.env" /etc/nockchain-jammer.env
    else
        cp "$INSTALL_DIR/.env.example" /etc/nockchain-jammer.env
    fi
fi

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

log_info "Creating systemd service..."
cat > "/etc/systemd/system/${SERVICE_NAME}.service" << EOF
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
