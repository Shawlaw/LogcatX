"""Local TCP fixtures for native Windows E2E; no device/network access."""
import json
import socket
import struct
import sys
import threading
import time
from pathlib import Path


def serve(listener, kind):
    while True:
        client, _ = listener.accept()
        threading.Thread(target=respond, args=(client, kind), daemon=True).start()


def respond(client, kind):
    with client:
        client.settimeout(2)
        try:
            data = b""
            while len(data) < 31:
                block = client.recv(31 - len(data))
                if not block:
                    return
                data += block
            if kind == "wireless" and data[:4] == b"CNXN":
                command = 0x534C5453
                client.sendall(struct.pack("<6I", command, 0x01000000, 0, 0, 0, command ^ 0xFFFFFFFF))
            else:
                client.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
        except (OSError, TimeoutError):
            pass


ports = {}
for kind in ("wireless", "http"):
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(16)
    ports[kind] = listener.getsockname()[1]
    threading.Thread(target=serve, args=(listener, kind), daemon=True).start()
Path(sys.argv[1]).write_text(json.dumps(ports), encoding="utf-8")
while True:
    time.sleep(1)
