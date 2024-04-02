#!/bin/sh

# create CA 
openssl req -x509 \
            -sha256 -days 356 \
            -nodes \
            -newkey rsa:2048 \
            -subj "/CN=MyOwnCA/C=US/L=San Fransisco" \
            -keyout rootCA.key -out rootCA.crt 

# create server private key
openssl genrsa -out server.key 2048

# create certificate signing request (CSR)
cat > csr.conf <<EOF
[ req ]
default_bits = 2048
prompt = no
default_md = sha256
req_extensions = req_ext
distinguished_name = dn

[ dn ]
C = US
ST = California
L = San Fransisco
O = Someone
OU = Someone
CN = localhost

[ req_ext ]
subjectAltName = @alt_names

[ alt_names ]
DNS.1 = localhost
EOF

openssl req -new -key server.key -out server.csr -config csr.conf

# create server cert
cat > cert.conf <<EOF
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, nonRepudiation, keyEncipherment, dataEncipherment
subjectAltName = @alt_names

[alt_names]
DNS.1 = localhost
EOF

openssl x509 -req \
    -in server.csr \
    -CA rootCA.crt -CAkey rootCA.key \
    -out server.crt \
    -days 365 \
    -sha256 -extfile cert.conf

# create pkcs12
openssl pkcs12 -export -out identity.pfx -inkey server.key -in server.crt -certfile rootCA.crt \
    -passout pass:1234 -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES

# clean up
rm server.csr csr.conf cert.conf
