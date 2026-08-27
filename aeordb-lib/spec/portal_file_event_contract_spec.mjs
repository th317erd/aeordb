'use strict';

import assert from 'node:assert/strict';
import test from 'node:test';

import {
  AuthorizedMutationEventTracker,
  reconcileAuthorizedPreviewEntry,
  validateRootedCollectionResponse,
} from '../src/portal/file-event-contract.mjs';

const PREVIOUS_ROOT_HASH = 'cd'.repeat(32);
const ROOT_HASH          = 'ab'.repeat(32);

function mutationEvent(overrides = {}) {
  return JSON.stringify({
    event_id:  '00000000-0000-4000-8000-000000000002',
    event_type: 'entries_created',
    timestamp:  1775968398000,
    payload: {
      operation_id:          '00000000-0000-4000-8000-000000000001',
      mutation_kind:         'file_write',
      publication_sequence:  42,
      previous_root_hash:    PREVIOUS_ROOT_HASH,
      root_hash:             ROOT_HASH,
      affected_relationships: [
        { path: '/docs/report.txt', entry_type: 'file', change: 'created' },
      ],
      ...overrides,
    },
  });
}

test('rooted collections require the coordinated root and exact collection name', () => {
  const response = {
    root:  { hash: ROOT_HASH, state: 'live', expires_at: null },
    items: [{ path: '/docs/report.txt' }],
    total: 1,
  };

  assert.equal(validateRootedCollectionResponse(response, 'items'), response);
  assert.throws(
    () => validateRootedCollectionResponse({ ...response, root: undefined }, 'items'),
    /root metadata/,
  );
  assert.throws(
    () => validateRootedCollectionResponse({ ...response, root: { ...response.root, hash: ROOT_HASH.toUpperCase() } }, 'items'),
    /root hash/,
  );
  assert.throws(
    () => validateRootedCollectionResponse({ root: response.root, results: response.items }, 'items'),
    /items collection/,
  );
});

test('authorized mutation projection accepts direct children and deduplicates acknowledgement fanout', () => {
  const tracker = new AuthorizedMutationEventTracker();
  const projected = tracker.projectForDirectory(mutationEvent({
    affected_relationships: [
      { path: '/docs/report.txt', entry_type: 'file', change: 'updated' },
      { path: '/docs/archive', entry_type: 'directory', change: 'created' },
    ],
  }), '/docs/');

  assert.equal(projected.root_hash, ROOT_HASH);
  assert.deepEqual(projected.affected_relationships.map((relationship) => relationship.path), [
    '/docs/report.txt',
    '/docs/archive',
  ]);
  assert.equal(tracker.projectForDirectory(mutationEvent(), '/docs/'), null);
});

test('authorized mutation projection ignores unrelated and nested descendants', () => {
  const unrelatedTracker = new AuthorizedMutationEventTracker();
  assert.equal(unrelatedTracker.projectForDirectory(mutationEvent(), '/images/'), null);

  const nestedTracker = new AuthorizedMutationEventTracker();
  assert.equal(nestedTracker.projectForDirectory(mutationEvent({
    affected_relationships: [
      { path: '/docs/nested/report.txt', entry_type: 'file', change: 'created' },
    ],
  }), '/docs/'), null);
});

test('authorized mutation projection fails closed on malformed envelopes', () => {
  const malformedEvents = [
    '{',
    mutationEvent({ affected_relationships: [] }),
    mutationEvent({ affected_relationships: [{ path: '/docs/../secret.txt', entry_type: 'file', change: 'created' }] }),
    mutationEvent({ affected_relationships: [{ path: '/docs/report.txt', entry_type: 'physical_file', change: 'created' }] }),
    mutationEvent({ affected_relationships: [{ path: '/docs/report.txt', entry_type: 'file', change: 'rewritten' }] }),
    mutationEvent({ publication_sequence: Number.MAX_SAFE_INTEGER + 1 }),
    mutationEvent({ previous_root_hash: '' }),
  ];

  for (const serializedEvent of malformedEvents) {
    const tracker = new AuthorizedMutationEventTracker();
    assert.equal(tracker.projectForDirectory(serializedEvent, '/docs/'), null);
  }
});

test('preview reconciliation hydrates only an explicitly affected preview', () => {
  const tab = {
    id:                'active',
    path:              '/docs/',
    preview_entry:     { path: '/docs/preview.txt', revision: 'old' },
    preview_component: 'text',
    entries:           [ { path: '/docs/preview.txt', revision: 'new' } ],
    _deletedEntries:   [],
  };
  let hydrateCount = 0;
  let showCount = 0;
  const helpers = {
    activeTabID: 'active',
    entryPath:   (_tab, entry) => entry.path,
    hydrate:     () => { hydrateCount += 1; },
    show:        () => { showCount += 1; },
  };

  reconcileAuthorizedPreviewEntry(tab, new Set([ '/docs/unrelated.txt' ]), helpers);
  assert.equal(tab.preview_entry.revision, 'old');
  assert.equal(hydrateCount, 0);
  assert.equal(showCount, 0);

  reconcileAuthorizedPreviewEntry(tab, new Set([ '/docs/preview.txt' ]), helpers);
  assert.equal(tab.preview_entry.revision, 'new');
  assert.equal(hydrateCount, 1);
  assert.equal(showCount, 0);

  tab.entries = [];
  reconcileAuthorizedPreviewEntry(tab, new Set([ '/docs/preview.txt' ]), helpers);
  assert.equal(tab.preview_entry, null);
  assert.equal(tab.preview_component, null);
  assert.equal(hydrateCount, 1);
  assert.equal(showCount, 1);
});
