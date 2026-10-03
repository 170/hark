#!/usr/bin/env python3
"""Reconnect after hark restarts. Usage: subscribe.py SOCKET [WORD ...]"""
import json
import socket
import sys
import time

path = sys.argv[1]
words = sys.argv[2:] or ["hey-computer"]
while True:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(path)
            client.sendall((json.dumps({"subscribe": words}) + "\n").encode("utf-8"))
            with client.makefile("r", encoding="utf-8") as messages:
                for line in messages:
                    message = json.loads(line)
                    if "error" in message:
                        raise SystemExit(message)
                    print(message, flush=True)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        print(f"disconnected: {error}", file=sys.stderr)
    time.sleep(1)
