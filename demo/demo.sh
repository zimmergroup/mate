#!/usr/bin/env bash
# A self-contained demo for screenshots and trying MATE out: two Mosquitto brokers in Docker
# (Plant A plain MQTT 3.1.1, Plant B TLS with MQTT 5) fed with invented plant data, plus a MATE
# configuration that points at them. Nothing here touches your own MATE configuration.
#
#   demo/demo.sh up       start the brokers, the publisher and write the MATE profiles
#   demo/demo.sh run      start MATE connected to both demo brokers
#   demo/demo.sh down     stop brokers and publisher
#   demo/demo.sh reset    down, and delete the demo folder
#
# Needs Docker, openssl, python3 and a mate binary in PATH (or set MATE=/path/to/mate).
set -euo pipefail

DIR="${MATE_DEMO_DIR:-/tmp/mate-demo}"
MATE="${MATE:-mate}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE="eclipse-mosquitto:2"
PORT_A=18830
PORT_B=18831

mate_cfg() { "$MATE" --config "$DIR/config.toml" "$@"; }

certs() {
  [ -f "$DIR/certs/server.pem" ] && return
  mkdir -p "$DIR/certs"
  openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj "/CN=MATE Demo CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -keyout "$DIR/certs/ca.key" -out "$DIR/certs/ca.pem" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
    -keyout "$DIR/certs/server.key" -out "$DIR/certs/server.csr" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\n' >"$DIR/certs/san.cnf"
  openssl x509 -req -in "$DIR/certs/server.csr" -CA "$DIR/certs/ca.pem" -CAkey "$DIR/certs/ca.key" \
    -CAcreateserial -days 365 -extfile "$DIR/certs/san.cnf" -out "$DIR/certs/server.pem" 2>/dev/null
  chmod 644 "$DIR/certs/server.key"
}

brokers() {
  printf 'listener 1883\nallow_anonymous true\n' >"$DIR/plant-a.conf"
  printf 'listener 8883\nallow_anonymous true\ncafile /certs/ca.pem\ncertfile /certs/server.pem\nkeyfile /certs/server.key\n' \
    >"$DIR/plant-b.conf"
  docker rm -f mate-demo-a mate-demo-b >/dev/null 2>&1 || true
  docker run -d --name mate-demo-a -p "127.0.0.1:$PORT_A:1883" \
    -v "$DIR/plant-a.conf:/mosquitto/config/mosquitto.conf:ro" "$IMAGE" >/dev/null
  docker run -d --name mate-demo-b -p "127.0.0.1:$PORT_B:8883" \
    -v "$DIR/plant-b.conf:/mosquitto/config/mosquitto.conf:ro" -v "$DIR/certs:/certs:ro" "$IMAGE" >/dev/null
}

profiles() {
  [ -f "$DIR/config.toml" ] && return
  mate_cfg broker add --name "Plant A" --host localhost --port "$PORT_A" --subscribe '#' --default
  mate_cfg broker edit plant-a --allow-publish true
  mate_cfg broker add --name "Plant B" --host localhost --port "$PORT_B" --protocol mqtt5 --tls --subscribe '#'
  mate_cfg broker cert-import plant-b ca "$DIR/certs/ca.pem"
  mate_cfg broker edit plant-b --allow-publish true
}

publisher() {
  if [ -f "$DIR/publisher.pid" ] && kill -0 "$(cat "$DIR/publisher.pid")" 2>/dev/null; then
    return
  fi
  nohup python3 "$HERE/publisher.py" --port-a "$PORT_A" --port-b "$PORT_B" --ca "$DIR/certs/ca.pem" \
    >"$DIR/publisher.log" 2>&1 &
  echo $! >"$DIR/publisher.pid"
}

stop() {
  [ -f "$DIR/publisher.pid" ] && kill "$(cat "$DIR/publisher.pid")" 2>/dev/null || true
  rm -f "$DIR/publisher.pid"
  docker rm -f mate-demo-a mate-demo-b >/dev/null 2>&1 || true
}

case "${1:-}" in
  up)
    mkdir -p "$DIR"
    certs
    brokers
    profiles
    publisher
    echo "Demo is running. Start MATE with: $0 run   (config folder: $DIR)"
    ;;
  run)
    exec "$MATE" --config "$DIR/config.toml" --broker "Plant A" --broker "Plant B" --connect
    ;;
  down)
    stop
    ;;
  reset)
    stop
    rm -rf "$DIR"
    ;;
  *)
    sed -n '2,12p' "$0"
    exit 1
    ;;
esac
