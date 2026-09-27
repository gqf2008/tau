#!/usr/bin/env python3
"""WeCom (企业微信) loopback platform for validate.sh step 5e
(docs/im-channels.md §企微回环协议).

Unlike wa_mock.py this mock speaks the REAL callback cryptography —
the whole point of the wecom adapter is that signature verification
and AES decryption are the component's job (the host is a pipe), so
the platform side must actually sign and encrypt:

- msg_signature = sha1 of the sorted [token, timestamp, nonce,
  encrypt_msg] concatenation, carried in the query string;
- AES-256-CBC, key = base64decode(EncodingAESKey + "="), IV = key[:16];
- plaintext frame = 16 random bytes + u32be(len) + msg + corpid;
- PKCS#7 padding with a 32-byte block size (wecom's quirk, not 16).

Python's stdlib has no AES, so a compact FIPS-197 implementation is
embedded below — and it PROVES ITSELF against NIST test vectors at
startup (the instrument testifies about itself first; a crypto mock
that silently mis-encrypts would produce hours of false debugging).

Legs (deliver thread, after the listener accepts):
  1. negative control: a tampered msg_signature MUST get 403;
  2. URL verification: GET with encrypted echostr, expect the
     plaintext echoed back (proves component-side verify+decrypt);
  3. encrypted text message POST, expect 200 "success".
The send API half (POST /cgi-bin/message/send) prints "WECOM SEND:".

Prints "wecom mock ready PORT" once listening.
"""

import argparse
import base64
import hashlib
import http.client
import os
import struct
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# ---------------------------------------------------------------------
# AES (FIPS-197), encryption direction only.
# ---------------------------------------------------------------------

SBOX = (
    0x63, 0x7C, 0x77, 0x7B, 0xF2, 0x6B, 0x6F, 0xC5, 0x30, 0x01, 0x67, 0x2B, 0xFE, 0xD7, 0xAB, 0x76,
    0xCA, 0x82, 0xC9, 0x7D, 0xFA, 0x59, 0x47, 0xF0, 0xAD, 0xD4, 0xA2, 0xAF, 0x9C, 0xA4, 0x72, 0xC0,
    0xB7, 0xFD, 0x93, 0x26, 0x36, 0x3F, 0xF7, 0xCC, 0x34, 0xA5, 0xE5, 0xF1, 0x71, 0xD8, 0x31, 0x15,
    0x04, 0xC7, 0x23, 0xC3, 0x18, 0x96, 0x05, 0x9A, 0x07, 0x12, 0x80, 0xE2, 0xEB, 0x27, 0xB2, 0x75,
    0x09, 0x83, 0x2C, 0x1A, 0x1B, 0x6E, 0x5A, 0xA0, 0x52, 0x3B, 0xD6, 0xB3, 0x29, 0xE3, 0x2F, 0x84,
    0x53, 0xD1, 0x00, 0xED, 0x20, 0xFC, 0xB1, 0x5B, 0x6A, 0xCB, 0xBE, 0x39, 0x4A, 0x4C, 0x58, 0xCF,
    0xD0, 0xEF, 0xAA, 0xFB, 0x43, 0x4D, 0x33, 0x85, 0x45, 0xF9, 0x02, 0x7F, 0x50, 0x3C, 0x9F, 0xA8,
    0x51, 0xA3, 0x40, 0x8F, 0x92, 0x9D, 0x38, 0xF5, 0xBC, 0xB6, 0xDA, 0x21, 0x10, 0xFF, 0xF3, 0xD2,
    0xCD, 0x0C, 0x13, 0xEC, 0x5F, 0x97, 0x44, 0x17, 0xC4, 0xA7, 0x7E, 0x3D, 0x64, 0x5D, 0x19, 0x73,
    0x60, 0x81, 0x4F, 0xDC, 0x22, 0x2A, 0x90, 0x88, 0x46, 0xEE, 0xB8, 0x14, 0xDE, 0x5E, 0x0B, 0xDB,
    0xE0, 0x32, 0x3A, 0x0A, 0x49, 0x06, 0x24, 0x5C, 0xC2, 0xD3, 0xAC, 0x62, 0x91, 0x95, 0xE4, 0x79,
    0xE7, 0xC8, 0x37, 0x6D, 0x8D, 0xD5, 0x4E, 0xA9, 0x6C, 0x56, 0xF4, 0xEA, 0x65, 0x7A, 0xAE, 0x08,
    0xBA, 0x78, 0x25, 0x2E, 0x1C, 0xA6, 0xB4, 0xC6, 0xE8, 0xDD, 0x74, 0x1F, 0x4B, 0xBD, 0x8B, 0x8A,
    0x70, 0x3E, 0xB5, 0x66, 0x48, 0x03, 0xF6, 0x0E, 0x61, 0x35, 0x57, 0xB9, 0x86, 0xC1, 0x1D, 0x9E,
    0xE1, 0xF8, 0x98, 0x11, 0x69, 0xD9, 0x8E, 0x94, 0x9B, 0x1E, 0x87, 0xE9, 0xCE, 0x55, 0x28, 0xDF,
    0x8C, 0xA1, 0x89, 0x0D, 0xBF, 0xE6, 0x42, 0x68, 0x41, 0x99, 0x2D, 0x0F, 0xB0, 0x54, 0xBB, 0x16,
)


def _xtimes(a):
    return ((a << 1) ^ (0x1B if a & 0x80 else 0)) & 0xFF


def _expand_key(key):
    """32-byte key -> 15 round keys of 16 bytes (AES-256, Nr=14)."""
    words = [list(key[4 * i:4 * i + 4]) for i in range(8)]
    rcon = 1
    for i in range(8, 60):
        t = words[i - 1][:]
        if i % 8 == 0:
            t = [SBOX[t[1]], SBOX[t[2]], SBOX[t[3]], SBOX[t[0]]]
            t[0] ^= rcon
            rcon = _xtimes(rcon)
        elif i % 8 == 4:
            t = [SBOX[b] for b in t]
        words.append([words[i - 8][j] ^ t[j] for j in range(4)])
    return [sum(words[4 * r:4 * r + 4], []) for r in range(15)]


def _encrypt_block(block, round_keys):
    s = list(block)  # column-major state: s[r + 4c]

    def add_rk(rk):
        for i in range(16):
            s[i] ^= rk[i]

    def shift_rows():
        s[1], s[5], s[9], s[13] = s[5], s[9], s[13], s[1]
        s[2], s[6], s[10], s[14] = s[10], s[14], s[2], s[6]
        s[3], s[7], s[11], s[15] = s[15], s[3], s[7], s[11]

    def mix_columns():
        for c in range(4):
            a0, a1, a2, a3 = s[4 * c:4 * c + 4]
            s[4 * c + 0] = _xtimes(a0) ^ _xtimes(a1) ^ a1 ^ a2 ^ a3
            s[4 * c + 1] = a0 ^ _xtimes(a1) ^ _xtimes(a2) ^ a2 ^ a3
            s[4 * c + 2] = a0 ^ a1 ^ _xtimes(a2) ^ _xtimes(a3) ^ a3
            s[4 * c + 3] = _xtimes(a0) ^ a0 ^ a1 ^ a2 ^ _xtimes(a3)

    add_rk(round_keys[0])
    for rnd in range(1, 14):
        for i in range(16):
            s[i] = SBOX[s[i]]
        shift_rows()
        mix_columns()
        add_rk(round_keys[rnd])
    for i in range(16):
        s[i] = SBOX[s[i]]
    shift_rows()
    add_rk(round_keys[14])
    return bytes(s)


def aes256_cbc_encrypt(key, iv, data):
    rks = _expand_key(key)
    out = b""
    prev = iv
    for off in range(0, len(data), 16):
        block = bytes(a ^ b for a, b in zip(data[off:off + 16], prev))
        prev = _encrypt_block(block, rks)
        out += prev
    return out


def _aes_self_test():
    """The instrument testifies about itself: FIPS-197 AES-256 example
    vector + the NIST SP 800-38A F.2.5 CBC-AES256 vector."""
    key = bytes.fromhex(
        "603deb1015ca71be2b73aef0857d7781"
        "1f352c073b6108d72d9810a30914dff4"
    )
    rks = _expand_key(key)
    pt = bytes.fromhex("6bc1bee22e409f96e93d7e117393172a")
    ct = _encrypt_block(pt, rks)
    assert ct.hex() == "f3eed1bdb5d2a03c064b5a7e3db181f8", \
        f"AES-256 ECB vector failed: {ct.hex()}"
    iv = bytes.fromhex("000102030405060708090a0b0c0d0e0f")
    pt4 = bytes.fromhex(
        "6bc1bee22e409f96e93d7e117393172a"
        "ae2d8a571e03ac9c9eb76fac45af8e51"
        "30c81c46a35ce411e5fbc1191a0a52ef"
        "f69f2445df4f9b17ad2b417be66c3710"
    )
    ct4 = aes256_cbc_encrypt(key, iv, pt4)
    assert ct4.hex() == (
        "f58c4c04d6e5f1ba779eabfb5f7bfbd6"
        "9cfc4e967edb808d679f777bc6702c7d"
        "39f23369a9d9bacfa530e26304231461"
        "b2eb05e2c39be9fcda6c19078c6a9d1b"
    ), f"AES-256-CBC vector failed: {ct4.hex()}"


# ---------------------------------------------------------------------
# WeCom callback cryptography (the real wire shapes).
# ---------------------------------------------------------------------

def wecom_sign(token, timestamp, nonce, encrypt_msg):
    """sha1 of the sorted concatenation — the msg_signature scheme."""
    return hashlib.sha1(
        "".join(sorted([token, timestamp, nonce, encrypt_msg])).encode()
    ).hexdigest()


def wecom_encrypt(key, corpid, msg):
    """Frame = 16 random + u32be(len) + msg + corpid, PKCS#7 with the
    wecom 32-byte block size, AES-256-CBC with IV = key[:16]."""
    plain = os.urandom(16) + struct.pack(">I", len(msg)) + msg + corpid.encode()
    pad = 32 - len(plain) % 32
    plain += bytes([pad]) * pad
    return base64.b64encode(
        aes256_cbc_encrypt(key, key[:16], plain)
    ).decode()


def xml_envelope(corpid, encrypt):
    return (
        "<xml><ToUserName><![CDATA[" + corpid + "]]></ToUserName>"
        "<Encrypt><![CDATA[" + encrypt + "]]></Encrypt>"
        "<AgentID><![CDATA[1000002]]></AgentID></xml>"
    )


INNER_TEXT = (
    "<xml><ToUserName><![CDATA[{corpid}]]></ToUserName>"
    "<FromUserName><![CDATA[loopback-user]]></FromUserName>"
    "<CreateTime>1777000000</CreateTime>"
    "<MsgType><![CDATA[text]]></MsgType>"
    "<Content><![CDATA[ping from the wecom platform]]></Content>"
    "<MsgId>7000000000000000001</MsgId>"
    "<AgentID>1000002</AgentID></xml>"
)


# ---------------------------------------------------------------------
# Delivery legs.
# ---------------------------------------------------------------------

def _request(method, url, body=None, headers=None):
    """One HTTP request against the component's ingress listener."""
    rest = url.split("://", 1)[1]
    hostport, _, path = rest.partition("/")
    conn = http.client.HTTPConnection(hostport, timeout=5)
    conn.request(method, "/" + path, body, headers or {})
    resp = conn.getresponse()
    data = resp.read().decode(errors="replace")
    conn.close()
    return resp.status, data


def deliver(webhook, token, aes_key, corpid, retry_seconds):
    key = base64.b64decode(aes_key + "=")
    timestamp = "1777000001"
    nonce = "loopback-nonce"

    # Wait for the listener (exists only after the bridge's
    # session_start ran). Use the leg-1 request itself as the probe.
    deadline = time.monotonic() + retry_seconds

    # Leg 1 — negative control: a tampered signature MUST be refused.
    good_encrypt = wecom_encrypt(key, corpid, INNER_TEXT.format(corpid=corpid).encode())
    bad_sig = wecom_sign(token, timestamp, nonce, "tampered-not-the-body")
    query = urllib.parse.urlencode({
        "msg_signature": bad_sig,
        "timestamp": timestamp,
        "nonce": nonce,
    })
    while True:
        try:
            status, _ = _request(
                "POST", webhook + "?" + query,
                xml_envelope(corpid, good_encrypt),
                {"content-type": "text/xml"},
            )
            break
        except OSError:
            if time.monotonic() > deadline:
                print("wecom mock: listener never came up", flush=True)
                return
            time.sleep(0.05)
    if status == 403:
        print("wecom mock: bad signature refused (403)", flush=True)
    else:
        print(f"wecom mock: BAD SIGNATURE NOT REFUSED (ack {status})", flush=True)
        return  # the crypto gate is broken; later legs prove nothing

    # Leg 2 — URL verification: encrypted echostr must come back plain.
    echostr_plain = b"loopback-echostr-42"
    echostr = wecom_encrypt(key, corpid, echostr_plain)
    query = urllib.parse.urlencode({
        "msg_signature": wecom_sign(token, timestamp, nonce, echostr),
        "timestamp": timestamp,
        "nonce": nonce,
        "echostr": echostr,
    })
    status, body = _request("GET", webhook + "?" + query)
    if status == 200 and body == echostr_plain.decode():
        print("wecom mock: url verify ok (echostr round-trip)", flush=True)
    else:
        print(f"wecom mock: url verify FAILED (ack {status} {body!r})", flush=True)
        return

    # Leg 3 — the encrypted text message.
    query = urllib.parse.urlencode({
        "msg_signature": wecom_sign(token, timestamp, nonce, good_encrypt),
        "timestamp": timestamp,
        "nonce": nonce,
    })
    status, body = _request(
        "POST", webhook + "?" + query,
        xml_envelope(corpid, good_encrypt),
        {"content-type": "text/xml"},
    )
    print(f"wecom mock: message delivered, ack: {status} {body}", flush=True)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        if self.path.startswith("/cgi-bin/message/send"):
            print(f"WECOM SEND: {body.decode(errors='replace')}", flush=True)
            payload = b'{"errcode":0,"errmsg":"ok"}'
            self.send_response(200)
        else:
            payload = b""
            self.send_response(404)
        self.send_header("content-length", str(len(payload)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--webhook", required=True, help="component ingress URL")
    parser.add_argument("--retry-seconds", type=float, default=15.0)
    parser.add_argument("--token", default="loopback-wecom-token")
    # base64 of bytes(range(16, 48)) — a fixed 32-byte loopback key.
    parser.add_argument(
        "--aes-key",
        default=base64.b64encode(bytes(range(16, 48))).decode().rstrip("="),
    )
    parser.add_argument("--corpid", default="loopback-corp")
    args = parser.parse_args()

    _aes_self_test()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    port = server.server_address[1]
    threading.Thread(
        target=deliver,
        args=(args.webhook, args.token, args.aes_key, args.corpid,
              args.retry_seconds),
        daemon=True,
    ).start()
    print(f"wecom mock ready {port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
