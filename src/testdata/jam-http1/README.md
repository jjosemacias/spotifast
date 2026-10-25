Synthetic, public test-only key and self-signed certificate, generated locally
for the H1 loopback tests. Never used by the application or installed in any
trust store. The fixture trusts this certificate only in its private in-memory
ClientConfig. SANs: 127.0.0.1 and spclient.wg.spotify.com (the latter allows
testing the unchanged request through a local synthetic CONNECT proxy, without
DNS or contacting that domain). Valid for ten years from 2026-10-24.

Generated with OpenSSL `req -x509 -newkey rsa:2048 -nodes -days 3650`, subject
`/CN=Jam HTTP1 synthetic test only`, and
`-addext subjectAltName=DNS:spclient.wg.spotify.com,IP:127.0.0.1`.
The leaf has `-addext basicConstraints=critical,CA:FALSE`.
Certificate exported as DER with `x509 -outform DER`; key as unencrypted PKCS8
DER with `pkcs8 -topk8 -nocrypt -outform DER`. No real credentials involved.
