'use strict';

const ROOT_HASH_PATTERN = /^(?:[0-9a-f]{64}|[0-9a-f]{128})$/;
const OPERATION_ID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const ROOT_STATES = new Set([ 'live', 'pending', 'retained' ]);
const MUTATION_KINDS = new Set([
  'file_write',
  'file_delete',
  'directory_create',
  'directory_delete',
  'symlink_write',
  'symlink_delete',
  'copy',
  'rename',
  'batch_write',
  'merge',
  'restore',
  'promote',
  'import',
  'sync_apply',
  'system_write',
  'plugin_write',
  'maintenance_repair',
]);
const RELATIONSHIP_ENTRY_TYPES = new Set([ 'file', 'directory', 'symlink' ]);
const RELATIONSHIP_CHANGES = new Set([ 'created', 'updated', 'deleted' ]);

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isRootHash(value) {
  return typeof value === 'string' && ROOT_HASH_PATTERN.test(value);
}

function validateRootMetadata(root) {
  if (!isObject(root))
    throw new Error('Response is missing exact root metadata');

  if (!isRootHash(root.hash))
    throw new Error('Response has an invalid root hash');

  if (!ROOT_STATES.has(root.state))
    throw new Error('Response has an invalid root state');

  if (root.state === 'pending') {
    if (!Number.isSafeInteger(root.expires_at) || root.expires_at < 0)
      throw new Error('Pending response root is missing a valid expiry');
  } else if (root.expires_at !== null) {
    throw new Error('Non-pending response root must have a null expiry');
  }
}

export function validateRootedCollectionResponse(response, collectionName) {
  if (!isObject(response))
    throw new Error('Response is not a rooted collection object');

  if (collectionName !== 'items' && collectionName !== 'results')
    throw new Error(`Unsupported rooted collection name: ${collectionName}`);

  validateRootMetadata(response.root);
  if (!Array.isArray(response[collectionName]))
    throw new Error(`Response is missing its ${collectionName} collection`);

  const alternateCollectionName = (collectionName === 'items') ? 'results' : 'items';
  if (Object.hasOwn(response, alternateCollectionName))
    throw new Error(`Response contains ambiguous ${alternateCollectionName} compatibility data`);

  return response;
}

function isCanonicalRelationshipPath(path) {
  if (typeof path !== 'string' || !path.startsWith('/'))
    return false;

  if (path.length > 1 && path.endsWith('/'))
    return false;

  const pathSegments = path.split('/');
  return !pathSegments.slice(1).some((segment) => segment.length === 0 || segment === '.' || segment === '..');
}

function decodeAffectedRelationship(value) {
  if (!isObject(value))
    return null;

  const allowedFields = new Set([ 'path', 'entry_type', 'change' ]);
  if (Object.keys(value).some((field) => !allowedFields.has(field)))
    return null;

  if (!isCanonicalRelationshipPath(value.path))
    return null;

  if (value.entry_type !== null && value.entry_type !== undefined && !RELATIONSHIP_ENTRY_TYPES.has(value.entry_type))
    return null;

  if (!RELATIONSHIP_CHANGES.has(value.change))
    return null;

  return value;
}

function decodeMutationEvent(serializedEvent) {
  let envelope;
  try {
    envelope = JSON.parse(serializedEvent);
  } catch (_) {
    return null;
  }

  if (!isObject(envelope) || !isObject(envelope.payload))
    return null;

  const payload = envelope.payload;
  if (!OPERATION_ID_PATTERN.test(payload.operation_id || ''))
    return null;

  if (!MUTATION_KINDS.has(payload.mutation_kind))
    return null;

  if (!Number.isSafeInteger(payload.publication_sequence) || payload.publication_sequence < 0)
    return null;

  if (!isRootHash(payload.previous_root_hash) || !isRootHash(payload.root_hash))
    return null;

  if (!Array.isArray(payload.affected_relationships) || payload.affected_relationships.length === 0)
    return null;

  const affectedRelationships = [];
  for (const value of payload.affected_relationships) {
    const relationship = decodeAffectedRelationship(value);
    if (!relationship)
      return null;

    affectedRelationships.push(relationship);
  }

  return {
    operation_id:          payload.operation_id,
    mutation_kind:         payload.mutation_kind,
    publication_sequence:  payload.publication_sequence,
    previous_root_hash:    payload.previous_root_hash,
    root_hash:             payload.root_hash,
    affected_relationships: affectedRelationships,
  };
}

function normalizeDirectoryPath(path) {
  if (typeof path !== 'string' || !path.startsWith('/') || path.includes('@search'))
    return null;

  const normalizedPath = path.replace(/\/{2,}/g, '/');
  return (normalizedPath === '/' || normalizedPath.endsWith('/')) ? normalizedPath : `${normalizedPath}/`;
}

function relationshipParentDirectory(path) {
  if (path === '/')
    return null;

  const finalSeparatorIndex = path.lastIndexOf('/');
  return (finalSeparatorIndex === 0) ? '/' : path.slice(0, finalSeparatorIndex + 1);
}

function relationshipAffectsDirectory(relationship, directoryPath) {
  const normalizedDirectoryPath = normalizeDirectoryPath(directoryPath);
  if (!normalizedDirectoryPath)
    return false;

  return relationshipParentDirectory(relationship.path) === normalizedDirectoryPath;
}

export function reconcileAuthorizedPreviewEntry(tab, affectedPreviewPaths, helpers) {
  if (!tab.preview_entry)
    return;

  const previewPath = helpers.entryPath(tab, tab.preview_entry);
  if (!affectedPreviewPaths.has(previewPath))
    return;

  const replacement = tab.entries.find((entry) => helpers.entryPath(tab, entry) === previewPath)
    || (tab._deletedEntries || []).find((entry) => helpers.entryPath(tab, entry) === previewPath);
  if (!replacement) {
    tab.preview_entry = null;
    tab.preview_component = null;
    helpers.show(tab);
    return;
  }

  tab.preview_entry = replacement;
  if (tab.id === helpers.activeTabID)
    helpers.hydrate();
}

export class AuthorizedMutationEventTracker {
  constructor(maximumRememberedOperations = 256) {
    if (!Number.isSafeInteger(maximumRememberedOperations) || maximumRememberedOperations < 1)
      throw new Error('maximumRememberedOperations must be a positive safe integer');

    this._maximumRememberedOperations = maximumRememberedOperations;
    this._rememberedOperationKeys      = new Set();
    this._rememberedOperationOrder     = [];
  }

  clear() {
    this._rememberedOperationKeys.clear();
    this._rememberedOperationOrder = [];
  }

  projectForDirectory(serializedEvent, directoryPath) {
    const mutation = decodeMutationEvent(serializedEvent);
    if (!mutation)
      return null;

    const operationKey = `${mutation.operation_id}:${mutation.publication_sequence}:${mutation.root_hash}`;
    if (this._rememberedOperationKeys.has(operationKey))
      return null;

    this._rememberOperation(operationKey);
    if (!mutation.affected_relationships.some((relationship) => relationshipAffectsDirectory(relationship, directoryPath)))
      return null;

    return mutation;
  }

  _rememberOperation(operationKey) {
    this._rememberedOperationKeys.add(operationKey);
    this._rememberedOperationOrder.push(operationKey);
    if (this._rememberedOperationOrder.length <= this._maximumRememberedOperations)
      return;

    const forgottenOperationKey = this._rememberedOperationOrder.shift();
    this._rememberedOperationKeys.delete(forgottenOperationKey);
  }
}
