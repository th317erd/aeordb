'use strict';

import '/shared/components/aeor-file-browser-portal.js';

class AeorFiles extends HTMLElement {
  connectedCallback() {
    if (!this._initialized) {
      this._initialized = true;
      this.innerHTML = '<aeor-file-browser-portal></aeor-file-browser-portal>';
    }
  }
}

customElements.define('aeor-files', AeorFiles);
