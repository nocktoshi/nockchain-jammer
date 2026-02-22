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

# Default values (can be overridden in .env)
REPO_URL="${REPO_URL:-https://github.com/nocktoshi/nockchain-jammer}"
INSTALL_DIR="${INSTALL_DIR:-/opt/nockchain-jammer}"
SERVICE_NAME="${SERVICE_NAME:-nockchain-jammer-api}"
API_PORT="${API_PORT:-3001}"
JAMS_DIR="${JAMS_DIR:-/usr/share/nginx/html/jams}"
SCRIPT_PATH="${SCRIPT_PATH:-/usr/local/bin/make-jam.sh}"
API_BINARY_PATH="${API_BINARY_PATH:-/usr/local/bin/nockchain-jammer-api}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if running as root
if [[ $EUID -ne 0 ]]; then
    log_error "This script must be run as root (use sudo)"
    exit 1
fi

log_info "Starting nockchain-jammer installation..."

# Safety checks
if [[ ! -f /etc/os-release ]] || ! grep -qi "ubuntu\|debian" /etc/os-release; then
    log_error "This installer is designed for Ubuntu/Debian systems only."
    log_error "For other systems, please follow the manual installation steps."
    exit 1
fi

# Install dependencies
log_info "Installing dependencies..."
apt-get update
apt-get install -y curl wget git build-essential pkg-config libssl-dev jq nginx

# Create jammer user early so we can build as this user
JAMMER_HOME="/var/lib/jammer"
if ! id jammer >/dev/null 2>&1; then
    useradd --system --shell /bin/bash --home "$JAMMER_HOME" --create-home jammer
fi
mkdir -p "$JAMMER_HOME"
chown jammer:jammer "$JAMMER_HOME"

# Install Rust via rustup for jammer user (system cargo from apt is too old)
JAMMER_RUSTUP="$JAMMER_HOME/.rustup"
JAMMER_CARGO_HOME="$JAMMER_HOME/.cargo"
JAMMER_CARGO="$JAMMER_CARGO_HOME/bin/cargo"
JAMMER_RUSTUP_BIN="$JAMMER_CARGO_HOME/bin/rustup"

if [[ ! -x "$JAMMER_CARGO" ]]; then
    log_info "Installing Rust toolchain for jammer user..."
    sudo -u jammer \
        RUSTUP_HOME="$JAMMER_RUSTUP" \
        CARGO_HOME="$JAMMER_CARGO_HOME" \
        bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path'
fi

# Ensure a default toolchain is configured
if [[ -x "$JAMMER_RUSTUP_BIN" ]]; then
    if ! sudo -u jammer RUSTUP_HOME="$JAMMER_RUSTUP" CARGO_HOME="$JAMMER_CARGO_HOME" \
        "$JAMMER_RUSTUP_BIN" show active-toolchain >/dev/null 2>&1; then
        log_info "Setting default Rust toolchain..."
        sudo -u jammer RUSTUP_HOME="$JAMMER_RUSTUP" CARGO_HOME="$JAMMER_CARGO_HOME" \
            "$JAMMER_RUSTUP_BIN" default stable
    fi
fi

if [[ ! -x "$JAMMER_CARGO" ]]; then
    log_error "Cargo not found at $JAMMER_CARGO after installation."
    log_error "Contents of $JAMMER_HOME: $(ls -la "$JAMMER_HOME" 2>&1)"
    exit 1
fi
log_info "Using cargo: $(sudo -u jammer RUSTUP_HOME="$JAMMER_RUSTUP" CARGO_HOME="$JAMMER_CARGO_HOME" "$JAMMER_CARGO" --version)"

# Clone or update repository
git config --global --add safe.directory "$INSTALL_DIR"
if [[ -d "$INSTALL_DIR/.git" ]]; then
    log_warn "Installation directory exists, updating..."
    git -C "$INSTALL_DIR" pull
else
    log_info "Cloning repository..."
    git clone "$REPO_URL" "$INSTALL_DIR"
fi

# Build the API binary as jammer user (not root)
log_info "Building API binary..."
chown -R jammer:jammer "$INSTALL_DIR"
sudo -u jammer \
    RUSTUP_HOME="$JAMMER_RUSTUP" \
    CARGO_HOME="$JAMMER_CARGO_HOME" \
    "$JAMMER_CARGO" build --release --manifest-path "$INSTALL_DIR/api/Cargo.toml"

# Install files
log_info "Installing files..."

# Stop service before updating binary (to avoid "Text file busy")
if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    log_info "Stopping existing service before updating binary..."
    systemctl stop "$SERVICE_NAME"
fi

# Install binary
cp "$INSTALL_DIR/api/target/release/nockchain-jammer-api" "$API_BINARY_PATH"
chmod +x "$API_BINARY_PATH"

# Install script
cp "$INSTALL_DIR/make-jam.sh" "$SCRIPT_PATH"
chmod +x "$SCRIPT_PATH"

# Install website files
mkdir -p "$JAMS_DIR"
cp "$INSTALL_DIR/website/index.html" "$JAMS_DIR/index.html"
cp "$INSTALL_DIR/website/style.css" "$JAMS_DIR/"
cp "$INSTALL_DIR/website/jam-icon.png" "$JAMS_DIR/" 2>/dev/null || true

# Set up runtime environment file
if [[ ! -f /etc/nockchain-jammer.env ]]; then
    log_info "Creating runtime configuration file..."
    # Copy .env if it exists, otherwise use .env.example
    if [[ -f "$INSTALL_DIR/.env" ]]; then
        cp "$INSTALL_DIR/.env" /etc/nockchain-jammer.env
        log_info "Copied custom configuration from .env"
    else
        cp "$INSTALL_DIR/.env.example" /etc/nockchain-jammer.env
        log_info "Copied default configuration from .env.example"
    fi
else
    log_info "Runtime configuration file already exists, preserving..."
fi

# Generate or preserve API key
if grep -q "^API_KEY=" /etc/nockchain-jammer.env && [[ -n "$(grep "^API_KEY=" /etc/nockchain-jammer.env | cut -d'=' -f2 | xargs)" ]]; then
    API_KEY=$(grep "^API_KEY=" /etc/nockchain-jammer.env | cut -d'=' -f2 | xargs)
    log_info "Using existing API key from /etc/nockchain-jammer.env: ${API_KEY:0:8}..."
elif [[ -n "${API_KEY:-}" ]]; then
    log_info "Using API key from .env file: ${API_KEY:0:8}..."
    # Ensure API_KEY line exists, then update it
    if grep -q "^API_KEY=" /etc/nockchain-jammer.env; then
        sed -i "s|^API_KEY=.*|API_KEY=$API_KEY|" /etc/nockchain-jammer.env
    else
        echo "API_KEY=$API_KEY" >> /etc/nockchain-jammer.env
    fi
else
    API_KEY=$(openssl rand -hex 32)
    log_info "Generated new API key: ${API_KEY:0:8}..."
    # Ensure API_KEY line exists, then update it
    if grep -q "^API_KEY=" /etc/nockchain-jammer.env; then
        sed -i "s|^API_KEY=.*|API_KEY=$API_KEY|" /etc/nockchain-jammer.env
    else
        echo "API_KEY=$API_KEY" >> /etc/nockchain-jammer.env
    fi
fi

# Create systemd service
log_info "Creating systemd service..."
cat > /etc/systemd/system/${SERVICE_NAME}.service << EOF
[Unit]
Description=Nockchain Jammer API
After=network.target

[Service]
Type=simple
User=jammer
EnvironmentFile=/etc/nockchain-jammer.env
ExecStart=$API_BINARY_PATH
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

# Set permissions
chown -R jammer:jammer "$JAMS_DIR"
mkdir -p /var/lib/jammer
chown jammer:jammer /var/lib/jammer

# Configure sudo for systemctl commands
log_info "Configuring sudo permissions..."
cat > /etc/sudoers.d/nockchain-jammer << EOF
jammer ALL=(ALL) NOPASSWD: /bin/systemctl stop nockchain, /bin/systemctl start nockchain, /bin/systemctl is-active nockchain
EOF
chmod 440 /etc/sudoers.d/nockchain-jammer

# Update make-jam.sh to use sudo
sed -i 's/systemctl /sudo systemctl /g' "$SCRIPT_PATH"

# Configure nginx
log_info "Configuring nginx..."
if [[ -f /etc/nginx/sites-available/default ]]; then
    # Backup existing config
    cp /etc/nginx/sites-available/default /etc/nginx/sites-available/default.backup.$(date +%Y%m%d_%H%M%S)

    # Add API proxy configuration
    if ! grep -q "location /api/" /etc/nginx/sites-available/default; then
        sed -i '/server {/a\
        location /api/ {\
            proxy_pass         http://127.0.0.1:'${API_PORT}';\
            proxy_http_version 1.1;\
            proxy_set_header   Host              $host;\
            proxy_set_header   X-Real-IP         $remote_addr;\
            proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;\
            proxy_set_header   X-Forwarded-Proto $scheme;\
            proxy_read_timeout 120s;\
            proxy_send_timeout 120s;\
        }\
' /etc/nginx/sites-available/default
    fi

    # Add static site configuration for jams
    if ! grep -q "location /jams/" /etc/nginx/sites-available/default; then
        cat >> /etc/nginx/sites-available/default << EOF

        location /jams/ {
            alias $JAMS_DIR/;
            index index.html;
            autoindex off;
            add_header Cache-Control "public, max-age=300";
            add_header X-Content-Type-Options nosniff;

            location ~* \.(jam|css|png|ico)$ {
                expires 1y;
                add_header Cache-Control "public, immutable";
            }

            location ~* SHA256SUMS {
                add_header Content-Type text/plain;
                add_header Cache-Control "no-cache";
            }
        }
EOF
    fi
else
    log_warn "Could not find default nginx config, you'll need to configure nginx manually"
fi

# Start services
log_info "Starting services..."
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"
systemctl start "$SERVICE_NAME"

# Test nginx config
if nginx -t 2>/dev/null; then
    systemctl reload nginx
    log_success "nginx configuration reloaded"
else
    log_warn "nginx configuration test failed, please check manually"
fi

# Test API
sleep 2
if curl -f -H "X-API-Key: $API_KEY" http://localhost:$API_PORT/api/status >/dev/null 2>&1; then
    log_success "API is responding correctly"
else
    log_warn "API test failed, check service status with: systemctl status $SERVICE_NAME"
fi

log_success "Installation complete!"
echo
echo "========================================"
echo "Installation Summary:"
echo "========================================"
echo "API Key: $API_KEY"
echo "API URL: http://localhost:$API_PORT"
echo "Jams Directory: $JAMS_DIR"
echo "Service: $SERVICE_NAME"
echo
echo "To test:"
echo "curl -H 'X-API-Key: $API_KEY' http://localhost:$API_PORT/api/status"
echo
echo "To trigger a jam build:"
echo "curl -X POST -H 'X-API-Key: $API_KEY' http://localhost:$API_PORT/api/make-jam"
echo
echo "View jams website at: http://your-server/jams/"
echo "========================================"