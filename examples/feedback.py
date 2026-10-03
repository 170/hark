#!/usr/bin/env python3
"""Report a detection or reset a word. Usage: feedback.py SOCKET WORD [EVENT_ID]"""
import argparse
import json
import socket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("socket")
    parser.add_argument("word")
    parser.add_argument("event_id", nargs="?")
    parser.add_argument("--label", choices=["false_positive", "true_positive"])
    parser.add_argument("--reset", action="store_true", help="Reset feedback adjustments for WORD")
    args = parser.parse_args()
    if args.reset:
        if args.event_id or args.label:
            parser.error("--reset cannot be combined with EVENT_ID or --label")
        request = {"reset": {"word": args.word}}
    else:
        if not args.event_id:
            parser.error("EVENT_ID is required unless --reset is used")
        request = {"feedback": {"event_id": args.event_id, "label": args.label or "false_positive"}}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(10)
        client.connect(args.socket)

        def send(message):
            client.sendall((json.dumps(message) + "\n").encode("utf-8"))

        send({"subscribe": [args.word]})
        with client.makefile("r", encoding="utf-8") as messages:
            for line in messages:
                message = json.loads(line)
                if "error" in message:
                    raise SystemExit(message)
                if message.get("event") == "ready":
                    send(request)
                elif message.get("event") == ("reset_result" if args.reset else "feedback_result"):
                    print(json.dumps(message, ensure_ascii=False), flush=True)
                    return
    raise SystemExit("Disconnected before the operation was acknowledged.")


if __name__ == "__main__":
    main()
