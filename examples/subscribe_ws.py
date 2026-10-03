#!/usr/bin/env python3
"""Subscribe over WebSocket, report an event, or reset feedback for a word."""
import argparse
import json
import sys
import time

from websockets.exceptions import ConnectionClosed, InvalidHandshake
from websockets.sync.client import connect


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", help="For example ws://hark:8765/ws")
    parser.add_argument("words", nargs="+", help="Wake-word identifiers")
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--feedback", metavar="EVENT_ID", help="Report a detection and exit")
    action.add_argument("--reset", action="store_true", help="Reset feedback for one word and exit")
    parser.add_argument("--label", choices=["false_positive", "true_positive"])
    args = parser.parse_args()
    if args.label and not args.feedback:
        parser.error("--label requires --feedback")
    if args.reset and len(args.words) != 1:
        parser.error("--reset requires exactly one word")
    while True:
        try:
            # Container traffic should go directly to hark, not an environment proxy.
            with connect(args.url, proxy=None, compression=None, close_timeout=2) as client:
                client.send(json.dumps({"subscribe": args.words}))
                for text in client:
                    message = json.loads(text)
                    if "error" in message:
                        raise SystemExit(message)
                    print(json.dumps(message, ensure_ascii=False), flush=True)
                    if message.get("event") == "ready" and args.feedback:
                        client.send(json.dumps({"feedback": {
                            "event_id": args.feedback, "label": args.label or "false_positive"
                        }}))
                    elif message.get("event") == "ready" and args.reset:
                        client.send(json.dumps({"reset": {"word": args.words[0]}}))
                    elif message.get("event") == "reset_result" and args.reset:
                        return
                    elif (message.get("event") == "feedback_result"
                          and message.get("event_id") == args.feedback):
                        return
        except InvalidHandshake as error:
            raise SystemExit(f"WebSocket handshake rejected: {error}") from error
        except (OSError, ConnectionClosed, TimeoutError, UnicodeError, json.JSONDecodeError) as error:
            print(f"disconnected: {error}", file=sys.stderr)
        if args.feedback or args.reset:
            raise SystemExit("Operation was not acknowledged; feedback reports can be retried with the same event ID.")
        time.sleep(1)


if __name__ == "__main__":
    main()
