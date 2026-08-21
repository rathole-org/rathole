# Integration tests

## TLS artifacts

TLS integration tests generate an isolated CA, server certificate, private
keys, PKCS#12 identity, and rendered config for each TLS scenario. The
temporary directory is removed when the scenario finishes.

The generated PKCS#12 encryption matches the selected backend: modern PBES2
for `native-tls`, and the legacy PBE format supported by the `rustls` loader.

Set `RATHOLE_TEST_KEEP_CERTS` to retain these files for debugging. Run tests
with `--nocapture` to see the retained directory path:

```sh
RATHOLE_TEST_KEEP_CERTS=1 cargo test --test integration_test tcp -- --nocapture
```
