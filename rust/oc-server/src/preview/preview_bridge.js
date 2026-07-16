  if (window.__openchamberPreviewBridgeInstalled) return;
  window.__openchamberPreviewBridgeInstalled = true;

  const SOURCE = 'openchamber-preview-bridge';
  const VERSION = 1;
  const MAX_TEXT = 500;
  const MAX_ARG = 1000;
  const TARGET_ORIGIN = typeof window.__openchamberPreviewTargetOrigin === 'string' ? window.__openchamberPreviewTargetOrigin : '';
  let inspectMode = false;
  let lastHoverKey = '';
  let pendingHover = null;
  let previewColorScheme = null;
  let nativeMatchMedia = null;
  const colorSchemeListeners = new Set();

  const parentOrigin = (() => {
    try {
      const ancestorOrigins = window.location && window.location.ancestorOrigins;
      const ancestorOrigin = ancestorOrigins && ancestorOrigins.length > 0 ? ancestorOrigins[0] : '';
      if (ancestorOrigin && ancestorOrigin !== 'null') return ancestorOrigin;
      const origin = document.referrer ? new URL(document.referrer).origin : '';
      return origin && origin !== 'null' ? origin : '';
    } catch {
      return '';
    }
  })();

  const post = (payload) => {
    try {
      if (parentOrigin && window.parent && typeof window.parent.postMessage === 'function') {
        const message = Object.assign({ source: SOURCE, version: VERSION }, payload || {});
        window.parent.postMessage(message, parentOrigin);
      }
    } catch {}
  };

  const clip = (value, max = MAX_TEXT) => {
    const text = String(value == null ? '' : value).replace(/\s+/g, ' ').trim();
    return text.length > max ? text.slice(0, max) + '...' : text;
  };

  const stringifyArg = (value) => {
    if (typeof value === 'string') return clip(value, MAX_ARG);
    if (value instanceof Error) return clip(value.stack || value.message || String(value), MAX_ARG);
    try {
      return clip(JSON.stringify(value), MAX_ARG);
    } catch {
      return clip(String(value), MAX_ARG);
    }
  };

  const normalizeColorScheme = (value) => value === 'dark' ? 'dark' : value === 'light' ? 'light' : null;

  const mediaQueryColorScheme = (query) => {
    const normalized = String(query || '').replace(/\s+/g, ' ').trim().toLowerCase();
    if (normalized === '(prefers-color-scheme: dark)') return 'dark';
    if (normalized === '(prefers-color-scheme: light)') return 'light';
    return null;
  };

  const mediaQueryMatchesPreviewScheme = (query) => {
    const scheme = mediaQueryColorScheme(query);
    if (!scheme || !previewColorScheme) return null;
    return previewColorScheme === scheme;
  };

  const notifyColorSchemeListeners = () => {
    for (const listener of Array.from(colorSchemeListeners)) {
      try {
        const matches = mediaQueryMatchesPreviewScheme(listener.media);
        if (matches === null) continue;
        const event = { matches, media: listener.media, type: 'change', target: listener.mql, currentTarget: listener.mql };
        listener.callback.call(listener.mql, event);
      } catch {}
    }
  };

  const installColorSchemeMatchMediaPatch = () => {
    if (window.__openchamberPreviewColorSchemePatched || typeof window.matchMedia !== 'function') return;
    window.__openchamberPreviewColorSchemePatched = true;
    nativeMatchMedia = window.matchMedia.bind(window);
    window.matchMedia = function(query) {
      const nativeMql = nativeMatchMedia(query);
      if (!mediaQueryColorScheme(query)) return nativeMql;
      const listenersForMql = new Map();
      const mql = Object.create(nativeMql);
      Object.defineProperty(mql, 'matches', { get: () => mediaQueryMatchesPreviewScheme(query) ?? nativeMql.matches });
      Object.defineProperty(mql, 'media', { get: () => nativeMql.media });
      mql.addEventListener = function(type, callback, options) {
        if (type !== 'change' || typeof callback !== 'function') return nativeMql.addEventListener?.(type, callback, options);
        const entry = { media: query, mql, callback };
        listenersForMql.set(callback, entry);
        colorSchemeListeners.add(entry);
      };
      mql.removeEventListener = function(type, callback, options) {
        if (type !== 'change' || typeof callback !== 'function') return nativeMql.removeEventListener?.(type, callback, options);
        const entry = listenersForMql.get(callback);
        if (entry) colorSchemeListeners.delete(entry);
        listenersForMql.delete(callback);
      };
      mql.addListener = function(callback) { mql.addEventListener('change', callback); };
      mql.removeListener = function(callback) { mql.removeEventListener('change', callback); };
      return mql;
    };
  };

  const shouldSyncDataTheme = () => {
    try {
      const root = document.documentElement;
      if (!root) return false;
      if (root.hasAttribute('data-theme')) return true;
      if (document.querySelector('starlight-theme-select, starlight-menu-button')) return true;
      const generator = document.querySelector('meta[name="generator"]');
      const generatorContent = generator && typeof generator.getAttribute === 'function' ? generator.getAttribute('content') || '' : '';
      if (generatorContent.toLowerCase().indexOf('starlight') >= 0) return true;
      const styles = window.getComputedStyle(root);
      return Boolean(styles.getPropertyValue('--sl-color-bg').trim()
        || styles.getPropertyValue('--sl-color-text').trim()
        || styles.getPropertyValue('--sl-color-accent').trim());
    } catch {
      return false;
    }
  };

  const applyPreviewColorScheme = (scheme) => {
    const next = normalizeColorScheme(scheme);
    if (!next || previewColorScheme === next) return;
    previewColorScheme = next;
    try {
      const root = document.documentElement;
      root.style.colorScheme = next;
      root.dataset.openchamberPreviewColorScheme = next;
      if (shouldSyncDataTheme()) {
        root.dataset.theme = next;
      }
    } catch {}
    notifyColorSchemeListeners();
  };

  const readElementUrl = (element) => {
    return element.currentSrc || element.src || element.href || element.action || '';
  };

  const upstreamPathForUrl = (value) => {
    try {
      const parsed = new URL(value, window.location.href);
      const match = parsed.pathname.match(/^\/api\/preview\/proxy\/[a-f0-9]{16,64}(\/.*)?$/i);
      return match ? (match[1] || '/') : parsed.pathname;
    } catch {
      return String(value || '');
    }
  };

  const upstreamPathAndSearchForUrl = (value) => {
    try {
      const parsed = new URL(value, window.location.href);
      const match = parsed.pathname.match(/^\/api\/preview\/proxy\/[a-f0-9]{16,64}(\/.*)?$/i);
      const path = match ? (match[1] || '/') : parsed.pathname;
      return path + parsed.search;
    } catch {
      return String(value || '');
    }
  };

  const isInternalDevToolResource = (element, value) => {
    const tag = element && element.tagName && typeof element.tagName.toLowerCase === 'function' ? element.tagName.toLowerCase() : '';
    if (tag !== 'script' && tag !== 'link') return false;
    if (tag === 'script' && typeof element.hasAttribute === 'function' && element.hasAttribute('data-cf-beacon')) return true;
    const pathAndSearch = upstreamPathAndSearchForUrl(value);
    const lower = pathAndSearch.toLowerCase();
    const path = pathAndSearch.split('?', 1)[0] || '';

    const viteNoise = path === '/@vite/client'
      || path === '/@react-refresh'
      || path.indexOf('/@id/__x00__vite/') === 0
      || lower.indexOf('/node_modules/.vite/') >= 0
      || lower.indexOf('/vite/dist/client/') >= 0
      || (tag === 'script' && lower.indexOf('/@id/') >= 0);
    const astroNoise = path.indexOf('/@id/astro:') === 0
      || lower.indexOf('/astro/dist/runtime/client/dev-toolbar/') >= 0
      || (tag === 'script' && lower.indexOf('.astro?') >= 0 && lower.indexOf('type=script') >= 0)
      || (tag === 'script' && (
        lower.endsWith('.css')
        || lower.indexOf('.css?') >= 0
        || lower.indexOf('type=style') >= 0
        || lower.indexOf('lang.css') >= 0
      ));
    const nextNoise = tag === 'script' && (
      path === '/_next/webpack-hmr'
      || lower.indexOf('/_next/static/webpack/') >= 0
      || lower.indexOf('/_next/static/chunks/webpack') >= 0
      || lower.indexOf('/_next/static/chunks/react-refresh') >= 0
      || lower.indexOf('/_next/static/development/') >= 0
    );
    const svelteKitNoise = tag === 'script' && (
      lower.indexOf('/@id/__x00__virtual:') >= 0
      || lower.indexOf('/@id/virtual:') >= 0
      || lower.indexOf('/.svelte-kit/generated/') >= 0
      || lower.indexOf('/node_modules/.vite/deps/') >= 0
    );
    const remixNoise = tag === 'script' && (
      lower.indexOf('/@remix-run/dev/') >= 0
      || lower.indexOf('/__manifest') >= 0
      || lower.indexOf('/__hmr') >= 0
    );
    const nuxtNoise = tag === 'script' && (
      lower.indexOf('/_nuxt/@vite/client') >= 0
      || lower.indexOf('/_nuxt/@id/') >= 0
      || lower.indexOf('/_nuxt/node_modules/.vite/') >= 0
      || lower.indexOf('/__nuxt_error') >= 0
      || lower.indexOf('/__nuxt_vite_node__') >= 0
    );
    const webpackNoise = tag === 'script' && (
      path === '/sockjs-node/info'
      || lower.indexOf('/webpack-dev-server/') >= 0
      || lower.indexOf('/webpack/hot/') >= 0
      || lower.indexOf('/__webpack_hmr') >= 0
      || (lower.indexOf('/ws') >= 0 && lower.indexOf('webpack') >= 0)
    );

    if (viteNoise || astroNoise || nextNoise || svelteKitNoise || remixNoise || nuxtNoise || webpackNoise) return true;
    return false;
  };

  installColorSchemeMatchMediaPatch();

  const classifyNavigation = (value) => {
    try {
      const parsed = new URL(value, window.location.href);
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return { action: 'allow', url: parsed.toString() };
      const current = new URL(window.location.href);
      const proxyMatch = current.pathname.match(/^(\/api\/preview\/proxy\/[a-f0-9]{16,64})(?:\/|$)/i);
      if (parsed.origin === current.origin && parsed.pathname === current.pathname && parsed.search === current.search && parsed.hash) {
        return { action: 'allow', url: parsed.toString() };
      }
      if (parsed.origin === current.origin && parsed.pathname.startsWith('/api/preview/proxy/')) {
        return { action: 'allow', url: parsed.toString() };
      }
      if (proxyMatch && parsed.origin === current.origin && parsed.pathname.startsWith('/') && parsed.pathname.indexOf(proxyMatch[1]) !== 0 && TARGET_ORIGIN) {
        try {
          const upstreamUrl = new URL(parsed.pathname + parsed.search + parsed.hash, TARGET_ORIGIN);
          return { action: 'proxy', url: upstreamUrl.toString() };
        } catch {}
      }
      const host = parsed.hostname;
      const isLoopback = host === 'localhost' || host === '127.0.0.1' || host === '0.0.0.0' || host === '::1' || host === '[::1]';
      if (isLoopback || (parsed.origin === current.origin && parsed.pathname.startsWith('/'))) {
        return { action: 'proxy', url: parsed.toString() };
      }
      return { action: 'external', url: parsed.toString() };
    } catch {
      return { action: 'allow', url: String(value || '') };
    }
  };

  const isInternalDevToolRuntimeError = (filename) => {
    const path = upstreamPathForUrl(filename || '');
    const pathAndSearch = upstreamPathAndSearchForUrl(filename || '');
    const lowerPathAndSearch = pathAndSearch.toLowerCase();
    const isStyleRuntimeNoise = lowerPathAndSearch.endsWith('.css')
      || lowerPathAndSearch.indexOf('.css?') >= 0
      || lowerPathAndSearch.indexOf('type=style') >= 0
      || lowerPathAndSearch.indexOf('lang.css') >= 0;
    return path === '/@vite/client'
      || path === '/@react-refresh'
      || path.indexOf('/astro/dist/runtime/client/dev-toolbar/') >= 0
      || path.indexOf('/node_modules/.vite/') >= 0
      || isStyleRuntimeNoise;
  };

  const isInternalDevToolConsoleNoise = (level, args) => {
    if (level !== 'error' || typeof args[0] !== 'string' || args[0].indexOf('[vite]') !== 0) return false;
    const text = args.map((arg) => stringifyArg(arg)).join(' ');
    return text.indexOf('failed to connect to websocket') >= 0
      || text.indexOf("Cannot read properties of undefined (reading 'send')") >= 0
      || text.indexOf('Cannot read properties of undefined (reading "send")') >= 0;
  };

  const installViteHmrProxyPatch = () => {
    if (window.__openchamberViteHmrProxyPatched || typeof window.WebSocket !== 'function') return;
    window.__openchamberViteHmrProxyPatched = true;
    const NativeWebSocket = window.WebSocket;
    const proxyMatch = window.location.pathname.match(/^(\/api\/preview\/proxy\/[a-f0-9]{16,64})(?:\/|$)/i);
    if (!proxyMatch) return;
    const proxyBase = proxyMatch[1] + '/';
    const currentSearchParams = new URL(window.location.href).searchParams;
    const previewToken = currentSearchParams.get('oc_preview_token') || '';
    const urlAuthToken = currentSearchParams.get('oc_url_token') || '';
    let reloadTimer = 0;

    const schedulePreviewReload = () => {
      if (reloadTimer) return;
      reloadTimer = window.setTimeout(() => {
        reloadTimer = 0;
        try {
          window.location.reload();
        } catch {}
      }, 80);
    };

    const rewriteUrl = (url, protocols) => {
      const protocolList = Array.isArray(protocols) ? protocols : [protocols];
      const isViteSocket = protocolList.indexOf('vite-hmr') >= 0 || protocolList.indexOf('vite-ping') >= 0;
      if (!isViteSocket) return url;
      try {
        const parsed = new URL(String(url), window.location.href);
        if (parsed.host !== window.location.host) return url;
        if (parsed.pathname.indexOf(proxyBase) !== 0) {
          parsed.pathname = proxyBase;
        }
        if (previewToken) parsed.searchParams.set('oc_preview_token', previewToken);
        if (urlAuthToken) parsed.searchParams.set('oc_url_token', urlAuthToken);
        return parsed.toString();
      } catch {
        return url;
      }
    };

    function OpenChamberPreviewWebSocket(url, protocols) {
      const protocolList = Array.isArray(protocols) ? protocols : [protocols];
      const isViteSocket = protocolList.indexOf('vite-hmr') >= 0;
      const nextUrl = rewriteUrl(url, protocols);
      const socket = arguments.length === 1
        ? new NativeWebSocket(nextUrl)
        : new NativeWebSocket(nextUrl, protocols);

      if (isViteSocket) {
        socket.addEventListener('message', (event) => {
          try {
            const payload = JSON.parse(String(event.data || ''));
            if (payload && (payload.type === 'update' || payload.type === 'full-reload')) {
              schedulePreviewReload();
            }
          } catch {}
        });
      }

      return socket;
    }

    OpenChamberPreviewWebSocket.prototype = NativeWebSocket.prototype;
    Object.setPrototypeOf(OpenChamberPreviewWebSocket, NativeWebSocket);
    Object.defineProperty(OpenChamberPreviewWebSocket, 'name', { value: 'WebSocket' });
    window.WebSocket = OpenChamberPreviewWebSocket;
  };

  const installAppRequestProxyPatch = () => {
    if (window.__openchamberAppRequestProxyPatched) return;
    window.__openchamberAppRequestProxyPatched = true;
    const proxyMatch = window.location.pathname.match(/^(\/api\/preview\/proxy\/[a-f0-9]{16,64})(?:\/|$)/i);
    if (!proxyMatch) return;
    const proxyBase = proxyMatch[1];
    const currentSearchParams = new URL(window.location.href).searchParams;
    const previewToken = currentSearchParams.get('oc_preview_token') || '';
    const urlAuthToken = currentSearchParams.get('oc_url_token') || '';

    const withProxyAuth = (value) => {
      if (typeof value !== 'string' || value.indexOf(proxyBase) !== 0) return value;
      if (!previewToken && !urlAuthToken) return value;
      try {
        const parsed = new URL(value, window.location.origin);
        parsed.searchParams.delete('oc_client_token');
        if (previewToken) parsed.searchParams.set('oc_preview_token', previewToken);
        if (urlAuthToken) parsed.searchParams.set('oc_url_token', urlAuthToken);
        return parsed.pathname + parsed.search + parsed.hash;
      } catch {
        return value;
      }
    };

    const shouldProxyPath = (pathname) => {
      if (typeof pathname !== 'string' || !pathname.startsWith('/') || pathname.startsWith('//')) return false;
      if (pathname.indexOf(proxyBase) === 0) return false;
      return true;
    };

    const proxiedUrl = (value) => {
      if (typeof value !== 'string') return value;
      if (value.startsWith('/')) {
        if (value.indexOf(proxyBase) === 0) return withProxyAuth(value);
        if (!shouldProxyPath(value)) return value;
        return withProxyAuth(proxyBase + value);
      }

      try {
        const parsed = new URL(value, window.location.href);
        if (parsed.origin === window.location.origin && shouldProxyPath(parsed.pathname)) {
          return withProxyAuth(proxyBase + parsed.pathname + parsed.search + parsed.hash);
        }
      } catch {}

      return value;
    };

    const proxiedWebSocketUrl = (value) => {
      if (typeof value !== 'string') return value;
      try {
        const parsed = new URL(value, window.location.href);
        const current = new URL(window.location.href);
        const sameHost = parsed.host === current.host;
        const isWebSocketProtocol = parsed.protocol === 'ws:' || parsed.protocol === 'wss:';
        if (sameHost && isWebSocketProtocol && shouldProxyPath(parsed.pathname)) {
          parsed.pathname = proxyBase + parsed.pathname;
          parsed.searchParams.delete('oc_client_token');
          if (previewToken) parsed.searchParams.set('oc_preview_token', previewToken);
          if (urlAuthToken) parsed.searchParams.set('oc_url_token', urlAuthToken);
          return parsed.toString();
        }
      } catch {}
      return value;
    };

    const proxiedNavigationUrl = (value) => {
      if (typeof value !== 'string') return value;
      try {
        const parsed = new URL(value, window.location.href);
        if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return value;
        if (parsed.origin === window.location.origin && parsed.pathname.indexOf(proxyBase) === 0) {
          return withProxyAuth(parsed.pathname + parsed.search + parsed.hash);
        }
        const host = parsed.hostname;
        const isLoopback = host === 'localhost' || host === '127.0.0.1' || host === '0.0.0.0' || host === '::1' || host === '[::1]';
        if (!isLoopback && parsed.origin !== window.location.origin) return value;
        if (!shouldProxyPath(parsed.pathname)) return value;
        return withProxyAuth(proxyBase + parsed.pathname + parsed.search + parsed.hash);
      } catch {
        return proxiedUrl(value);
      }
    };

    if (window.history && typeof window.history.pushState === 'function') {
      const nativePushState = window.history.pushState.bind(window.history);
      window.history.pushState = function(state, unused, url) {
        return nativePushState(state, unused, url === undefined ? url : proxiedNavigationUrl(String(url)));
      };
    }

    if (window.history && typeof window.history.replaceState === 'function') {
      const nativeReplaceState = window.history.replaceState.bind(window.history);
      window.history.replaceState = function(state, unused, url) {
        return nativeReplaceState(state, unused, url === undefined ? url : proxiedNavigationUrl(String(url)));
      };
    }

    if (typeof window.fetch === 'function') {
      const nativeFetch = window.fetch.bind(window);
      window.fetch = function(input, init) {
        if (typeof input === 'string') {
          return nativeFetch(proxiedUrl(input), init);
        }
        if (input instanceof Request) {
          try {
            const parsed = new URL(input.url);
            if (parsed.origin === window.location.origin && shouldProxyPath(parsed.pathname)) {
              const nextUrl = withProxyAuth(proxyBase + parsed.pathname + parsed.search + parsed.hash);
              return nativeFetch(new Request(nextUrl, input), init);
            }
          } catch {}
        }
        return nativeFetch(input, init);
      };
    }

    if (window.XMLHttpRequest && window.XMLHttpRequest.prototype) {
      const nativeOpen = window.XMLHttpRequest.prototype.open;
      window.XMLHttpRequest.prototype.open = function(method, url) {
        const args = Array.prototype.slice.call(arguments);
        if (typeof url === 'string') {
          args[1] = proxiedUrl(url);
        }
        return nativeOpen.apply(this, args);
      };
    }

    if (typeof window.EventSource === 'function') {
      const NativeEventSource = window.EventSource;
      function OpenChamberPreviewEventSource(url, eventSourceInitDict) {
        return new NativeEventSource(proxiedUrl(String(url)), eventSourceInitDict);
      }
      OpenChamberPreviewEventSource.prototype = NativeEventSource.prototype;
      Object.setPrototypeOf(OpenChamberPreviewEventSource, NativeEventSource);
      Object.defineProperty(OpenChamberPreviewEventSource, 'name', { value: 'EventSource' });
      window.EventSource = OpenChamberPreviewEventSource;
    }

    if (typeof window.WebSocket === 'function') {
      const NativeWebSocket = window.WebSocket;
      function OpenChamberPreviewAppWebSocket(url, protocols) {
        const nextUrl = proxiedWebSocketUrl(String(url));
        return arguments.length === 1
          ? new NativeWebSocket(nextUrl)
          : new NativeWebSocket(nextUrl, protocols);
      }
      OpenChamberPreviewAppWebSocket.prototype = NativeWebSocket.prototype;
      Object.setPrototypeOf(OpenChamberPreviewAppWebSocket, NativeWebSocket);
      Object.defineProperty(OpenChamberPreviewAppWebSocket, 'name', { value: 'WebSocket' });
      window.WebSocket = OpenChamberPreviewAppWebSocket;
    }
  };

  const selectorPart = (element) => {
    const tag = element.tagName.toLowerCase();
    if (element.id && /^[A-Za-z][\w:.-]*$/.test(element.id)) return tag + '#' + CSS.escape(element.id);
    const testId = element.getAttribute('data-testid') || element.getAttribute('data-test') || element.getAttribute('data-cy');
    if (testId) return tag + '[data-testid="' + CSS.escape(testId) + '"]';
    const classes = Array.from(element.classList || []).slice(0, 3).map((entry) => '.' + CSS.escape(entry)).join('');
    return tag + classes;
  };

  const buildSelector = (element) => {
    const parts = [];
    let current = element;
    while (current && current.nodeType === Node.ELEMENT_NODE && current !== document.documentElement) {
      let part = selectorPart(current);
      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter((child) => child.tagName === current.tagName);
        if (siblings.length > 1 && !part.includes('#') && !part.includes('[data-testid=')) {
          part += ':nth-of-type(' + (siblings.indexOf(current) + 1) + ')';
        }
      }
      parts.unshift(part);
      if (part.includes('#')) break;
      current = parent;
    }
    return parts.join(' > ');
  };

  const metadataForElement = (element) => {
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return null;
    const rect = element.getBoundingClientRect();
    const style = window.getComputedStyle(element);
    const attributes = {};
    for (const name of ['id', 'class', 'role', 'aria-label', 'href', 'src', 'data-testid', 'data-test', 'data-cy']) {
      const value = typeof element.getAttribute === 'function' ? element.getAttribute(name) : null;
      if (value) attributes[name] = clip(value, 300);
    }
    const ancestry = [];
    let current = element;
    while (current && current.nodeType === Node.ELEMENT_NODE && ancestry.length < 6) {
      ancestry.unshift({
        tag: current.tagName.toLowerCase(),
        id: current.id || undefined,
        className: clip(current.className || '', 200) || undefined,
        selectorPart: selectorPart(current),
      });
      current = current.parentElement;
    }
    return {
      frame: 'top',
      tag: element.tagName.toLowerCase(),
      text: clip(element.innerText || element.textContent || ''),
      selector: buildSelector(element),
      path: ancestry.map((entry) => entry.tag).join(' > '),
      bounds: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
      center: { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 },
      attributes,
      computedStyle: {
        display: style.display,
        position: style.position,
        color: style.color,
        backgroundColor: style.backgroundColor,
        fontFamily: style.fontFamily,
        fontSize: style.fontSize,
        fontWeight: style.fontWeight,
        lineHeight: style.lineHeight,
        zIndex: style.zIndex,
      },
      ancestry,
    };
  };

  const hoverKeyForTarget = (target) => {
    if (!target) return '';
    const bounds = target.bounds || {};
    return [target.selector, Math.round(bounds.x), Math.round(bounds.y), Math.round(bounds.width), Math.round(bounds.height)].join('|');
  };

  const sendHover = (event) => {
    if (!inspectMode) return;
    pendingHover = event;
    if (window.__openchamberPreviewHoverFrame) return;
    window.__openchamberPreviewHoverFrame = window.requestAnimationFrame(() => {
      window.__openchamberPreviewHoverFrame = 0;
      const currentEvent = pendingHover;
      pendingHover = null;
      if (!currentEvent || !inspectMode) return;
      const element = document.elementFromPoint(currentEvent.clientX, currentEvent.clientY);
      const target = metadataForElement(element);
      const key = hoverKeyForTarget(target);
      if (key === lastHoverKey) return;
      lastHoverKey = key;
      post({ type: 'hover', target, pointer: { x: currentEvent.clientX, y: currentEvent.clientY }, ts: Date.now() });
    });
  };

  const setInspectMode = (enabled) => {
    inspectMode = Boolean(enabled);
    lastHoverKey = '';
    document.documentElement.style.cursor = inspectMode ? 'crosshair' : '';
    if (!inspectMode) {
      post({ type: 'hover', target: null, pointer: { x: 0, y: 0 }, ts: Date.now() });
    }
  };

  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    const original = console[level];
    console[level] = function() {
      const args = Array.prototype.slice.call(arguments);
      if (level === 'debug' && typeof args[0] === 'string' && args[0].indexOf('[vite]') === 0) {
        return original.apply(console, args);
      }
      if (isInternalDevToolConsoleNoise(level, args)) {
        return original.apply(console, args);
      }
      post({ type: 'console', level, args: args.map(stringifyArg), ts: Date.now() });
      return original.apply(console, args);
    };
  }

  installViteHmrProxyPatch();
  installAppRequestProxyPatch();

  window.addEventListener('error', (event) => {
    const target = event.target;
    if (target && target !== window && target.nodeType === Node.ELEMENT_NODE) {
      const url = readElementUrl(target);
      if (isInternalDevToolResource(target, url)) {
        return;
      }
      post({
        type: 'resource-error',
        tag: target.tagName.toLowerCase(),
        url: clip(url, 1000),
        outerHTML: clip(target.outerHTML || '', 1000),
        ts: Date.now(),
      });
      return;
    }
    if (isInternalDevToolRuntimeError(event.filename)) {
      return;
    }
    post({
      type: 'runtime-error',
      message: clip(event.message || 'Unknown error', 1000),
      stack: clip(event.error && event.error.stack ? event.error.stack : '', 2000) || undefined,
      filename: event.filename,
      line: event.lineno,
      column: event.colno,
      ts: Date.now(),
    });
  }, true);

  window.addEventListener('unhandledrejection', (event) => {
    post({
      type: 'runtime-error',
      message: clip(event.reason && event.reason.message ? event.reason.message : event.reason || 'Unhandled promise rejection', 1000),
      stack: clip(event.reason && event.reason.stack ? event.reason.stack : '', 2000) || undefined,
      ts: Date.now(),
    });
  });

  window.addEventListener('message', (event) => {
    if (event.source !== window.parent) return;
    const data = event.data;
    if (!data || data.source !== 'openchamber-preview-parent' || data.version !== VERSION) return;
    if (data.type === 'set-inspect-mode') {
      setInspectMode(data.enabled === true);
    }
    if (data.type === 'set-color-scheme') {
      applyPreviewColorScheme(data.scheme);
    }
  });

  window.addEventListener('mousemove', sendHover, true);
  window.addEventListener('mouseleave', () => {
    if (!inspectMode) return;
    lastHoverKey = '';
    post({ type: 'hover', target: null, pointer: { x: 0, y: 0 }, ts: Date.now() });
  }, true);
  window.addEventListener('click', (event) => {
    const anchor = event.target && typeof event.target.closest === 'function' ? event.target.closest('a[href]') : null;
    if (anchor && !inspectMode) {
      const navigation = classifyNavigation(anchor.href);
      if (navigation.action === 'proxy' || navigation.action === 'external') {
        event.preventDefault();
        event.stopPropagation();
        post({ type: 'navigate-preview', url: navigation.url, navigation: navigation.action, ts: Date.now() });
        return;
      }
    }

    if (!inspectMode) return;
    event.preventDefault();
    event.stopPropagation();
    const element = document.elementFromPoint(event.clientX, event.clientY);
    const target = metadataForElement(element);
    if (target) {
      post({ type: 'select', target, pointer: { x: event.clientX, y: event.clientY }, ts: Date.now() });
    }
  }, true);

  window.addEventListener('DOMContentLoaded', () => {
    post({ type: 'ready', url: window.location.href, title: document.title || '' });
  });
  post({ type: 'ready', url: window.location.href, title: document.title || '' });
})();
