#!/usr/bin/env python3
import os
import sys
path = os.environ.get('PATH_INFO','/')
# Read stdin (body) until EOF
body = sys.stdin.read()
print("Content-Type: text/html")
print()
print(f"<html><body><h1>CGI Hello</h1><p>PATH_INFO: {path}</p><pre>{body}</pre></body></html>")
