'use strict';

import '/shared/components/aeor-file-browser-portal.js';
import {
  AuthorizedMutationEventTracker,
  reconcileAuthorizedPreviewEntry,
  validateRootedCollectionResponse,
} from '/shared/file-event-contract.mjs';

const AeorFileBrowserPortal = customElements.get('aeor-file-browser-portal');
if (!AeorFileBrowserPortal)
  throw new Error('The shared AeorDB file browser component is unavailable');

class AeorDBFileBrowserPortal extends AeorFileBrowserPortal {
  constructor() {
    super();
    this._authorizedMutationEventTracker = new AuthorizedMutationEventTracker();
  }

  async browse(path, limit, offset, sort, order) {
    const cleanPath = (path && path !== '/') ? path.replace(/^\//, '') : null;
    const filesPath = (cleanPath) ? `/files/${cleanPath}` : '/files/%2F';
    const queryParameters = new URLSearchParams({
      limit:  String(limit),
      offset: String(offset),
    });
    if (sort)
      queryParameters.set('sort', sort);
    if (order)
      queryParameters.set('order', order);

    const response = await window.api(`${filesPath}?${queryParameters.toString()}`);
    const data = await this._readRootedResponse(response, 'items', 'Browse');
    return {
      root:    data.root,
      entries: data.items.map((item) => ({
        name:                  item.name,
        path:                  item.path,
        entry_type:            item.entry_type,
        size:                  item.size || 0,
        content_type:          item.content_type || 'application/octet-stream',
        created_at:            item.created_at,
        updated_at:            item.updated_at,
        effective_permissions: item.effective_permissions || null,
      })),
      total: data.total,
    };
  }

  async search(query, limit, offset) {
    const response = await window.api('/files/search', {
      method:  'POST',
      headers: { 'Content-Type': 'application/json' },
      body:    JSON.stringify({
        query,
        path:   '/',
        limit:  limit || 100,
        offset: offset || 0,
      }),
    });
    const data = await this._readRootedResponse(response, 'results', 'Search');
    return {
      root:    data.root,
      entries: data.results.map((item) => {
        const path = item.path || '/';
        const cleanPath = path.replace(/\/+$/, '');
        const finalSeparatorIndex = cleanPath.lastIndexOf('/');
        const name = cleanPath.split('/').filter(Boolean).pop() || path;
        const parentPath = (finalSeparatorIndex > 0) ? cleanPath.slice(0, finalSeparatorIndex + 1) : '/';
        return {
          name,
          path,
          _actual_path:          path,
          _search_path_label:    parentPath,
          entry_type:            item.entry_type || (path.endsWith('/') ? 1 : 2),
          size:                  item.size || 0,
          content_type:          item.content_type || 'application/octet-stream',
          created_at:            item.created_at,
          updated_at:            item.updated_at,
          effective_permissions: item.effective_permissions || null,
        };
      }),
      total: data.total_count,
    };
  }

  async _readRootedResponse(response, collectionName, operationName) {
    if (!response.ok) {
      const error = new Error(`${operationName} failed: ${response.status}`);
      error.status = response.status;
      error.category = (response.status >= 500) ? 'upstream_server' : 'upstream_rejected';
      throw error;
    }

    try {
      return validateRootedCollectionResponse(await response.json(), collectionName);
    } catch (cause) {
      const error = new Error(`${operationName} returned an invalid rooted response: ${cause.message}`);
      error.category = 'upstream_protocol';
      throw error;
    }
  }

  async _loadTabEntries(tab, path, limit, offset) {
    const data = await super._loadTabEntries(tab, path, limit, offset);
    tab.root = data.root;
    return data;
  }

  _connectSSE() {
    if (this._eventSource)
      return;

    const token = (window.AUTH) ? window.AUTH.token : null;
    if (!token && window.aeordbAuthDisabled !== true)
      return;

    const eventTypes = [ 'entries_created', 'entries_updated', 'entries_deleted' ];
    let url = `/system/events?events=${encodeURIComponent(eventTypes.join(','))}`;
    if (token)
      url += `&token=${encodeURIComponent(token)}`;
    try {
      this._eventSource = new EventSource(url);
      for (const eventType of eventTypes)
        this._eventSource.addEventListener(eventType, (event) => this._onAuthorizedMutation(event));

      this._eventSource.addEventListener('stream_gap', () => this._onAuthorizedMutationGap());
      this._eventSource.onerror = () => {
        if ((window.AUTH && window.AUTH.token) || window.aeordbAuthDisabled === true)
          return;

        this._eventSource.close();
        this._eventSource = null;
      };
    } catch (_) {
      this._eventSource = null;
    }
  }

  _onAuthorizedMutation(event) {
    const tab = this._activeTab();
    if (!tab)
      return;

    const mutation = this._authorizedMutationEventTracker.projectForDirectory(event.data, tab.path);
    if (!mutation)
      return;

    if (!(tab._authorizedMutationPreviewPaths instanceof Set))
      tab._authorizedMutationPreviewPaths = new Set();

    for (const relationship of mutation.affected_relationships)
      tab._authorizedMutationPreviewPaths.add(relationship.path);

    tab._authorizedMutationRoot = {
      previous_hash: mutation.previous_root_hash,
      hash:          mutation.root_hash,
      sequence:      mutation.publication_sequence,
    };
    this._scheduleBackgroundListingRefresh(tab);
  }

  _onAuthorizedMutationGap() {
    this._authorizedMutationEventTracker.clear();
    const tab = this._activeTab();
    if (tab)
      tab._authorizedMutationPreviewPaths = null;

    this.refreshActiveListingFromEvent();
  }

  refreshActiveListingFromEvent(options = {}) {
    if (options.invalidateSharedPaths)
      this._sharedPathData = null;

    const tab = this._activeTab();
    if (!tab)
      return;

    if (options.preservePreview)
      tab._authorizedMutationPreviewPaths = new Set();

    this._scheduleBackgroundListingRefresh(tab);
  }

  _scheduleBackgroundListingRefresh(tab) {
    if (!tab)
      return;

    if (tab.id !== this._active_tab_id) {
      tab._needsRefresh = true;
      return;
    }

    if (tab._authorizedListingRefreshScheduled)
      return;

    tab._authorizedListingRefreshScheduled = true;
    queueMicrotask(() => {
      tab._authorizedListingRefreshScheduled = false;
      this._refreshListingInBackground(tab).catch((error) => {
        console.warn('Authorized listing refresh failed:', error);
      });
    });
  }

  _refreshPreviewEntry(tab) {
    const affectedPreviewPaths = tab._authorizedMutationPreviewPaths;
    if (!(affectedPreviewPaths instanceof Set)) {
      super._refreshPreviewEntry(tab);
      return;
    }

    tab._authorizedMutationPreviewPaths = null;
    reconcileAuthorizedPreviewEntry(tab, affectedPreviewPaths, {
      activeTabID: this._active_tab_id,
      entryPath:   (currentTab, entry) => this._entryPath(currentTab, entry),
      hydrate:     () => this._hydratePreview(),
      show:        (currentTab) => this._showPreview(currentTab),
    });
  }
}

customElements.define('aeordb-file-browser-portal-v1', AeorDBFileBrowserPortal);

class AeorFiles extends HTMLElement {
  connectedCallback() {
    if (this._initialized)
      return;

    this._initialized = true;
    this.appendChild(document.createElement('aeordb-file-browser-portal-v1'));
  }
}

customElements.define('aeor-files', AeorFiles);
