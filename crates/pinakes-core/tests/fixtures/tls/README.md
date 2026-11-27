A certificate chain for the relay TLS test (`resolver::egress`): `ca.pem`, a test CA, and
`relay.pem` with its key `relay.key`, the certificate it issued for `relay.test`, valid until
2126. Generated with OpenSSL for the tests only; nothing outside them trusts the CA, and the key
protects nothing.
