# Integration tests

## Scenario deadlines

Every integration scenario has a 60-second deadline. TLS artifact generation
runs on a dedicated thread under a separate 60-second setup deadline.
Timeout errors identify the config and phase, then managed client and server
tasks are shut down before the test returns.

## TLS artifacts

TLS integration tests generate an isolated CA, server certificate, private
keys, PKCS#12 identity, and rendered config for each TLS scenario. The
temporary directory is removed when the scenario finishes.

The generated identity matches the documented example: RSA-2048 with 3DES
PKCS#12 encryption and a SHA-256 MAC. Both TLS backends test this format.

Set `RATHOLE_TEST_KEEP_CERTS` to retain these files for debugging. Run tests
with `--nocapture` to see the retained directory path:

```sh
RATHOLE_TEST_KEEP_CERTS=1 cargo test --test integration_test \
  common::tls::tests::generates_and_cleans_up_tls_artifacts -- --exact --nocapture
```
