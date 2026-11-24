#!/bin/bash
# Yousj browser's Chromium launcher: vanilla Chrome-for-Testing 158
# (installed for the cookie-injector tool) plus cert bypass for the
# sandbox egress proxy's re-signed HTTPS certificates.
exec /home/hatch/workspace/cookie-injector/browsers/chrome/linux-158.0.8127.36/chrome-linux64/chrome --ignore-certificate-errors --ignore-ssl-errors "$@"
