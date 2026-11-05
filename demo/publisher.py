#!/usr/bin/env python3
"""Publishes invented plant data to the two demo brokers (standard library only).

Plant A: plain MQTT 3.1.1.  Plant B: TLS with MQTT 5 user properties.
"""

import argparse
import json
import math
import random
import socket
import ssl
import struct
import time
from datetime import datetime, timezone


def varint(value):
    out = bytearray()
    while True:
        byte = value % 128
        value //= 128
        out.append(byte | 0x80 if value else byte)
        if not value:
            return bytes(out)


def text(value):
    data = value.encode()
    return struct.pack(">H", len(data)) + data


class Client:
    def __init__(self, host, port, version, ca=None):
        self.host, self.port, self.version, self.ca = host, port, version, ca
        self.sock = None

    def connect(self):
        sock = socket.create_connection((self.host, self.port), timeout=5)
        if self.ca:
            context = ssl.create_default_context(cafile=self.ca)
            sock = context.wrap_socket(sock, server_hostname=self.host)
        body = text("MQTT") + bytes([self.version, 0x02]) + struct.pack(">H", 0)
        if self.version == 5:
            body += b"\x00"
        body += text(f"demo-publisher-{random.randrange(1 << 24):06x}")
        sock.sendall(b"\x10" + varint(len(body)) + body)
        sock.recv(64)  # the CONNACK; nothing else is read from the broker
        self.sock = sock

    def close(self):
        if self.sock:
            self.sock.close()
        self.sock = None

    def publish(self, topic, payload, retain=False, properties=()):
        if isinstance(payload, str):
            payload = payload.encode()
        body = text(topic)
        if self.version == 5:
            props = b"".join(b"\x26" + text(k) + text(v) for k, v in properties)
            body += varint(len(props)) + props
        body += payload
        self.sock.sendall(bytes([0x30 | int(retain)]) + varint(len(body)) + body)


def now():
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


def walk(value, step, low, high):
    return min(high, max(low, value + random.uniform(-step, step)))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port-a", type=int, default=18830)
    parser.add_argument("--port-b", type=int, default=18831)
    parser.add_argument("--ca", required=True)
    args = parser.parse_args()

    a = Client(args.host, args.port_a, 4)
    b = Client(args.host, args.port_b, 5, args.ca)
    states = ["RUNNING", "RUNNING", "RUNNING", "IDLE", "RUNNING", "MAINTENANCE"]
    temp1, temp2, speed, battery, hall = 182.0, 176.0, 1.2, 87.0, 21.5
    cycle = 0
    tick = 0
    while True:
        tick += 1
        temp1 = walk(temp1, 1.5, 150, 220)
        temp2 = walk(temp2, 1.5, 150, 220)
        speed = walk(speed, 0.05, 0.4, 2.0)
        battery = max(5.0, battery - 0.02)
        hall = walk(hall, 0.1, 18, 27)

        try:
            if a.sock is None:
                a.connect()
            cycle += 1
            a.publish("plant-a/line1/press01/telemetry", json.dumps({
                "timestamp": now(), "temperature_c": round(temp1, 1),
                "pressure_bar": round(120 + 10 * math.sin(tick / 5), 1),
                "cycle": cycle, "state": "RUNNING"}))
            if tick % 2 == 0:
                a.publish("plant-a/line1/press02/telemetry", json.dumps({
                    "timestamp": now(), "temperature_c": round(temp2, 1),
                    "pressure_bar": round(118 + 8 * math.cos(tick / 7), 1),
                    "cycle": cycle // 2, "state": "RUNNING"}))
            if tick % 10 == 1:
                a.publish("plant-a/line1/press01/state", random.choice(states), retain=True)
            a.publish("plant-a/line2/conveyor01/speed", f"{speed:.2f}")
            if tick % 25 == 0:
                a.publish("plant-a/line2/conveyor01/alarm", json.dumps({
                    "code": random.choice(["E102", "E215", "W007"]),
                    "severity": random.choice(["warning", "error"]),
                    "message": "Belt tension low", "acknowledged": False}))
            if tick % 3 == 0:
                volts = [round(230 + random.uniform(-2, 2), 1) for _ in range(3)]
                a.publish("plant-a/energy/meter01", json.dumps({
                    "timestamp": now(),
                    "phases": [{"l": i + 1, "v": v, "a": round(random.uniform(8, 16), 1)}
                               for i, v in enumerate(volts)],
                    "total_kw": round(random.uniform(30, 45), 2)}))
            if tick % 20 == 0:
                a.publish("plant-a/diagnostics/log", random.choice(
                    ["boot ok", "{broken json", "service restarted", "[1, 2,"]))
            if tick % 60 == 1:
                a.publish("plant-a/maintenance/firmware",
                          bytes([0x7F, 0x45, 0x4C, 0x46, 2, 1, 1, 0, 0xDE, 0xAD, 0xBE, 0xEF]),
                          retain=True)
            a.publish("shared/heartbeat", json.dumps({"seq": tick, "site": "A"}))
        except OSError:
            a.close()

        try:
            if b.sock is None:
                b.connect()
            props = [("source", "shuttle01"), ("site", "B")]
            b.publish("plant-b/warehouse/shuttle01/position", json.dumps({
                "timestamp": now(), "x": round(random.uniform(0, 40), 2),
                "y": round(random.uniform(0, 12), 2),
                "zone": random.choice(["inbound", "storage", "outbound"])}),
                properties=props)
            b.publish("plant-b/warehouse/shuttle01/battery", f"{battery:.1f}", properties=props)
            b.publish("plant-b/climate/hall1/temperature", f"{hall:.1f}")
            if tick % 2 == 0:
                b.publish("plant-b/climate/hall1/humidity", f"{random.uniform(38, 55):.0f}")
            if tick % 15 == 0:
                b.publish("plant-b/orders/new", json.dumps({
                    "order": f"SO-{1000 + tick}", "customer": "Example Ltd.",
                    "lines": [{"sku": "A-100", "qty": random.randint(1, 20)},
                              {"sku": "B-220", "qty": random.randint(1, 5)}]}),
                    properties=[("channel", "web")])
            b.publish("shared/heartbeat", json.dumps({"seq": tick, "site": "B"}))
        except OSError:  # ssl.SSLError is an OSError
            b.close()
        time.sleep(1)


if __name__ == "__main__":
    main()
