# Integration tests

## TLS artifacts

TLS integration tests generate an isolated CA, server certificate, private
keys, PKCS#12 identity, and rendered config for each TLS scenario. The
temporary directory is removed when the scenario finishes.

Set `RATHOLE_TEST_KEEP_CERTS` to retain these files for debugging. Run tests
with `--nocapture` to see the retained directory path:

```sh
RATHOLE_TEST_KEEP_CERTS=1 cargo test --test integration_test tcp -- --nocapture
```
