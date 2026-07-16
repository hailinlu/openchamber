#!/usr/bin/env bun
/**
 * Generate frozen JSON fixture for cross-compatibility byte-vector tests.
 * Uses the JS host-side relay modules (packages/web/server/lib/relay/) to
 * produce deterministic output bytes that the Rust test reads and asserts
 * against its own implementation.
 *
 * Run from repo root: bun run scripts/generate-relay-fixtures.mjs
 */

import { writeFileSync, mkdirSync } from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

import {
  createHostHandshake,
  bytesToBase64Url,
} from '../packages/web/server/lib/relay/e2ee.js';
import {
  encodeTunnelFrame as jsEncode,
  encodeFrameBatch as jsEncodeBatch,
  TunnelFrameType as JsFrameType,
} from '../packages/web/server/lib/relay/tunnel-codec.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PROJECT_ROOT = path.resolve(__dirname, '..');

// ---------------------------------------------------------------------------
// 1. Generate key material
// ---------------------------------------------------------------------------
const subtle = globalThis.crypto.subtle;

// Host long-lived ECDH keypair
const hostKeys = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
const hostPubJwkRaw = await subtle.exportKey('jwk', hostKeys.publicKey);
const hostPrivJwkRaw = await subtle.exportKey('jwk', hostKeys.privateKey);
const hostPubJwk = { kty: hostPubJwkRaw.kty, crv: hostPubJwkRaw.crv, x: hostPubJwkRaw.x, y: hostPubJwkRaw.y };
const hostPrivJwk = { kty: hostPrivJwkRaw.kty, crv: hostPrivJwkRaw.crv, x: hostPrivJwkRaw.x, y: hostPrivJwkRaw.y, d: hostPrivJwkRaw.d };

// Client ephemeral ECDH keypair
const clientKeys = await subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']);
const clientPubJwkRaw = await subtle.exportKey('jwk', clientKeys.publicKey);
const clientPrivJwkRaw = await subtle.exportKey('jwk', clientKeys.privateKey);
const clientPubJwk = { kty: clientPubJwkRaw.kty, crv: clientPubJwkRaw.crv, x: clientPubJwkRaw.x, y: clientPubJwkRaw.y };
const clientPrivJwk = { kty: clientPrivJwkRaw.kty, crv: clientPrivJwkRaw.crv, x: clientPrivJwkRaw.x, y: clientPrivJwkRaw.y, d: clientPrivJwkRaw.d };

// ---------------------------------------------------------------------------
// 2. Deterministic nonce (0x00..0x0f)
// ---------------------------------------------------------------------------
const nonce = new Uint8Array(16);
for (let i = 0; i < 16; i++) nonce[i] = i;
const nonceB64 = bytesToBase64Url(nonce);

// ---------------------------------------------------------------------------
// 3. Handshake JSON strings (match TS createClientHandshake structure)
// ---------------------------------------------------------------------------

// Hello with batch:true
const helloBatch = JSON.stringify({
  t: 'hello',
  v: 1,
  clientPubJwk: { kty: clientPubJwk.kty, crv: clientPubJwk.crv, x: clientPubJwk.x, y: clientPubJwk.y },
  nonce: nonceB64,
  batch: true,
});

// Hello without batch field
const helloNoBatch = JSON.stringify({
  t: 'hello',
  v: 1,
  clientPubJwk: { kty: clientPubJwk.kty, crv: clientPubJwk.crv, x: clientPubJwk.x, y: clientPubJwk.y },
  nonce: nonceB64,
});

// Feed hellos to JS host handshake to produce ready messages.
// Each permutation uses a FRESH host handshake instance so state does not leak
// (receiving a second hello with the same key returns SendText, not Established).
const hostBatch = createHostHandshake(hostKeys.privateKey, { batch: true });
const actionBatch = await hostBatch.handleText(helloBatch);

const hostForNoBatchClient = createHostHandshake(hostKeys.privateKey, { batch: true });
const actionClientNoBatch = await hostForNoBatchClient.handleText(helloNoBatch);

const hostForNoBatchHost = createHostHandshake(hostKeys.privateKey, { batch: false });
const actionHostNoBatch = await hostForNoBatchHost.handleText(helloBatch);

if (
  actionBatch.type !== 'established' ||
  actionClientNoBatch.type !== 'established' ||
  actionHostNoBatch.type !== 'established'
) {
  throw new Error(
    `Handshake fixture generation failed: batch=${actionBatch.type}, ` +
    `clientNoBatch=${actionClientNoBatch.type}, hostNoBatch=${actionHostNoBatch.type}`,
  );
}

const readyBatch = actionBatch.replyText;
const readyNoBatch = actionClientNoBatch.replyText;
const readyNoBatchHost = actionHostNoBatch.replyText;

// ---------------------------------------------------------------------------
// 4. Tunnel frame encoding
// ---------------------------------------------------------------------------
const tunnelPayload = new TextEncoder().encode('{"method":"GET"}');
const tunnelFrame = jsEncode(JsFrameType.HttpRequest, 5, tunnelPayload);

// Also encode individual batch frames
const frameAlpha = jsEncode(JsFrameType.HttpBody, 1, new TextEncoder().encode('alpha'));
const frameBeta  = jsEncode(JsFrameType.HttpBody, 1, new TextEncoder().encode('beta'));
const frameGamma = jsEncode(JsFrameType.HttpBody, 1, new TextEncoder().encode('gamma'));
const frameOnly  = jsEncode(JsFrameType.HttpBody, 1, new TextEncoder().encode('only'));

// ---------------------------------------------------------------------------
// 5. Batch envelope encoding
// ---------------------------------------------------------------------------
const singleBatch = jsEncodeBatch([frameOnly]);
const multiBatch  = jsEncodeBatch([frameAlpha, frameBeta, frameGamma]);

// ---------------------------------------------------------------------------
// 6. Build fixture
// ---------------------------------------------------------------------------
const fixture = {
  version: 1,
  source: 'JS host (e2ee.js + tunnel-codec.js)',
  generated_at: new Date().toISOString(),

  keys: {
    host: { publicJwk: hostPubJwk, privateJwk: hostPrivJwk },
    client: { publicJwk: clientPubJwk, privateJwk: clientPrivJwk },
  },

  nonce_b64: nonceB64,

  handshake: {
    hello_batch: helloBatch,
    hello_no_batch: helloNoBatch,
    ready_batch: readyBatch,
    ready_no_batch: readyNoBatch,
    ready_no_batch_from_nobatch_host: readyNoBatchHost,
  },

  tunnel_frame: {
    frame_type: 1,
    stream_id: 5,
    payload_b64: bytesToBase64Url(tunnelPayload),
    encoded_b64: bytesToBase64Url(tunnelFrame),
  },

  batch: {
    single_frame_b64: bytesToBase64Url(singleBatch),
    single_raw_frame_b64: bytesToBase64Url(frameOnly),
    multi_frame_b64: bytesToBase64Url(multiBatch),
    multi_raw_frames: [
      bytesToBase64Url(frameAlpha),
      bytesToBase64Url(frameBeta),
      bytesToBase64Url(frameGamma),
    ],
  },
};

// ---------------------------------------------------------------------------
// 7. Write fixture
// ---------------------------------------------------------------------------
const fixtureDir = path.resolve(PROJECT_ROOT, 'rust/oc-server/src/relay/tests/fixtures');
mkdirSync(fixtureDir, { recursive: true });
const fixturePath = path.join(fixtureDir, 'cross_compat_vectors.json');
writeFileSync(fixturePath, JSON.stringify(fixture, null, 2) + '\n');
console.log(`Fixture written to ${fixturePath}`);
console.log(`  Host pub JWK x: ${hostPubJwk.x.slice(0, 16)}...`);
console.log(`  Client pub JWK x: ${clientPubJwk.x.slice(0, 16)}...`);
console.log(`  Nonce b64: ${nonceB64}`);
console.log(`  Tunnel frame (${tunnelFrame.length} bytes): ${bytesToBase64Url(tunnelFrame).slice(0, 20)}...`);
