/* ==========================================================================
 * 文件服务器 · HTTP 浏览页脚本
 *
 * 说明：
 *  - 纯原生 JavaScript，无任何依赖、无 import / module 语法、无外部 URL；
 *  - 页面数据通过 /_api/list 获取，动态文本一律使用 textContent 写入，防止 XSS；
 *  - 上传按契约「每个文件一次 XMLHttpRequest + 原始请求体」实现；
 *  - 接口契约见同目录 README.md。
 * ========================================================================== */
(function () {
  'use strict';

  /* ------------------------------------------------------------------------
   * 常量
   * ---------------------------------------------------------------------- */

  var API = {
    list: '/_api/list',
    upload: '/_api/upload',
    mkdir: '/_api/mkdir'
  };

  var SORT_KEYS = ['name', 'size', 'modified', 'ext'];

  var SORT_LABELS = {
    name: '名称',
    size: '大小',
    modified: '修改时间',
    ext: '类型'
  };

  // 分类顺序即页面分组顺序：文件夹固定在最前，其他排在最后。
  var CATEGORY_DEFS = [
    { id: 'folder', name: '文件夹', icon: '📁' },
    { id: 'image', name: '图片', icon: '🖼️' },
    { id: 'video', name: '视频', icon: '🎬' },
    { id: 'audio', name: '音频', icon: '🎵' },
    { id: 'document', name: '文档', icon: '📄' },
    { id: 'archive', name: '压缩包', icon: '📦' },
    { id: 'code', name: '代码', icon: '💻' },
    { id: 'program', name: '程序', icon: '⚙️' },
    { id: 'font', name: '字体', icon: '🔤' },
    { id: 'other', name: '其他', icon: '📎' }
  ];

  var EXT_GROUPS = {
    image: ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'svg', 'ico', 'tif', 'tiff', 'avif', 'heic'],
    video: ['mp4', 'mkv', 'avi', 'mov', 'wmv', 'flv', 'webm', 'm4v', 'mpg', 'mpeg', '3gp', 'ts'],
    audio: ['mp3', 'wav', 'flac', 'aac', 'ogg', 'm4a', 'wma', 'opus', 'mid', 'midi'],
    document: ['pdf', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'txt', 'md', 'rtf', 'odt', 'ods', 'csv', 'epub', 'mobi'],
    archive: ['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'tgz', 'zst', 'iso'],
    code: ['rs', 'js', 'ts', 'tsx', 'jsx', 'py', 'java', 'c', 'h', 'cpp', 'hpp', 'cs', 'go', 'rb', 'php', 'html', 'htm', 'css', 'scss', 'json', 'xml', 'yaml', 'yml', 'toml', 'sh', 'bat', 'ps1', 'sql', 'ini', 'conf'],
    program: ['exe', 'msi', 'dll', 'so', 'dylib', 'apk', 'deb', 'rpm', 'jar', 'wasm'],
    font: ['ttf', 'otf', 'woff', 'woff2']
  };

  // 同一后缀出现在多个分类时，以 CATEGORY_DEFS 中靠前的分类为准（如 ts → 视频）。
  var EXT_TO_CATEGORY = (function () {
    var map = Object.create(null);
    CATEGORY_DEFS.forEach(function (category) {
      var extensions = EXT_GROUPS[category.id];
      if (!extensions) return;
      for (var index = 0; index < extensions.length; index += 1) {
        var ext = extensions[index];
        if (!Object.prototype.hasOwnProperty.call(map, ext)) map[ext] = category.id;
      }
    });
    return map;
  })();

  var CATEGORY_BY_ID = (function () {
    var map = Object.create(null);
    CATEGORY_DEFS.forEach(function (category) { map[category.id] = category; });
    return map;
  })();

  /* ------------------------------------------------------------------------
   * 状态
   * ---------------------------------------------------------------------- */

  var state = {
    dir: '/',                      // 当前虚拟目录（已解码，无末尾斜杠，根为 "/"）
    entries: [],
    writable: false,
    dataReady: false,
    loading: false,
    lastError: null,
    loadToken: 0,
    view: 'grouped',               // grouped | flat
    sortKey: 'name',
    sortOrder: 'asc',
    collapsed: Object.create(null)
  };

  var el = {};
  var dragDepth = 0;
  var uploadSerial = 0;
  var uploadSession = null;
  var mkdirState = { pending: false, focusBack: null };
  var NAME_COLLATOR = createNameCollator();

  /* ------------------------------------------------------------------------
   * 基础工具
   * ---------------------------------------------------------------------- */

  function $(id) {
    return document.getElementById(id);
  }

  function createNameCollator() {
    try {
      if (typeof Intl !== 'undefined' && Intl.Collator) {
        return new Intl.Collator('zh-Hans-CN', { numeric: true, sensitivity: 'base' });
      }
    } catch (error) {
      /* 忽略：回退到字符串比较 */
    }
    return null;
  }

  function compareName(left, right) {
    var a = left == null ? '' : String(left);
    var b = right == null ? '' : String(right);
    if (NAME_COLLATOR) return NAME_COLLATOR.compare(a, b);
    if (a < b) return -1;
    if (a > b) return 1;
    return 0;
  }

  function safeDecode(text) {
    try {
      return decodeURIComponent(text);
    } catch (error) {
      return text;
    }
  }

  function safeDecodePath(path) {
    return String(path || '').split('/').map(safeDecode);
  }

  function splitDir(dir) {
    return String(dir || '/').split('/').filter(function (part) { return part.length > 0; });
  }

  function joinDir(segments) {
    return segments.length ? '/' + segments.join('/') : '/';
  }

  function dirFromPathname(pathname) {
    var segments = [];
    String(pathname || '/').split('/').forEach(function (part) {
      if (!part) return;
      segments.push(safeDecode(part));
    });
    return joinDir(segments);
  }

  function dirUrl(segments) {
    if (!segments.length) return '/';
    return '/' + segments.map(function (part) { return encodeURIComponent(part); }).join('/') + '/';
  }

  function segmentsFromEncodedPath(encodedPath) {
    return String(encodedPath || '').split('/').filter(function (part) {
      return part.length > 0;
    }).map(safeDecode);
  }

  function pad2(value) {
    return value < 10 ? '0' + value : String(value);
  }

  function formatSize(bytes) {
    var value = typeof bytes === 'number' && isFinite(bytes) && bytes > 0 ? bytes : 0;
    if (value < 1024) return value + ' B';
    var units = ['KB', 'MB', 'GB', 'TB', 'PB'];
    var index = -1;
    do {
      value = value / 1024;
      index += 1;
    } while (value >= 1024 && index < units.length - 1);
    var digits = value >= 100 ? 0 : 1;
    return value.toFixed(digits) + ' ' + units[index];
  }

  function formatTime(seconds) {
    if (typeof seconds !== 'number' || !isFinite(seconds)) return '—';
    var date = new Date(seconds * 1000);
    if (isNaN(date.getTime())) return '—';
    return date.getFullYear() + '-' + pad2(date.getMonth() + 1) + '-' + pad2(date.getDate()) +
      ' ' + pad2(date.getHours()) + ':' + pad2(date.getMinutes());
  }

  function fileSize(file) {
    return file && typeof file.size === 'number' && file.size > 0 ? file.size : 0;
  }

  function parseJsonSafe(text) {
    if (!text) return null;
    try {
      return JSON.parse(text);
    } catch (error) {
      return null;
    }
  }

  function describeException(error) {
    if (!error) return '未知错误';
    return error.message || String(error);
  }

  function describeStatus(status) {
    if (status === 400) return '请求参数不正确';
    if (status === 403) return '没有权限执行该操作（服务器可能为只读模式）';
    if (status === 404) return '目标不存在';
    if (status === 409) return '目标已存在';
    if (status === 500) return '服务器处理请求时出错';
    if (!status) return '网络错误，无法连接服务器';
    return '请求失败（HTTP ' + status + '）';
  }

  function makeHttpError(status, payload, fallback) {
    var message = payload && typeof payload.error === 'string' && payload.error ? payload.error : '';
    var error = new Error(message || fallback || describeStatus(status));
    error.status = status;
    return error;
  }

  function truncateText(text, maxLength) {
    var value = String(text == null ? '' : text);
    if (value.length <= maxLength) return value;
    return value.slice(0, maxLength - 1) + '…';
  }

  function sliceFileList(list) {
    var result = [];
    if (!list) return result;
    for (var index = 0; index < list.length; index += 1) result.push(list[index]);
    return result;
  }

  /* ------------------------------------------------------------------------
   * 条目归一化 / 分类 / 排序
   * ---------------------------------------------------------------------- */

  function extFromName(name) {
    var base = String(name || '');
    var dot = base.lastIndexOf('.');
    if (dot <= 0 || dot === base.length - 1) return '';
    return base.slice(dot + 1).toLowerCase();
  }

  function normalizeEntry(raw) {
    if (!raw || typeof raw !== 'object') return null;
    var isDir = raw.is_dir === true;
    var name = typeof raw.name === 'string' ? raw.name : '';
    if (!name) return null;

    var ext = '';
    if (!isDir) {
      ext = String(raw.ext == null ? '' : raw.ext).toLowerCase().replace(/^\.+/, '').trim();
      if (!ext) ext = extFromName(name);
    }

    var modified = typeof raw.modified === 'number' && isFinite(raw.modified) ? raw.modified : null;
    var size = !isDir && typeof raw.size === 'number' && isFinite(raw.size) && raw.size > 0 ? raw.size : 0;

    return {
      name: name,
      path: typeof raw.path === 'string' ? raw.path : '',
      isDir: isDir,
      size: isDir ? 0 : size,
      modified: modified,
      ext: ext,
      category: isDir ? 'folder' : (EXT_TO_CATEGORY[ext] || 'other')
    };
  }

  function categoryOf(entry) {
    return entry && entry.category ? entry.category : 'other';
  }

  function categoryMeta(categoryId) {
    return CATEGORY_BY_ID[categoryId] || CATEGORY_BY_ID.other;
  }

  function modifiedValue(entry) {
    return typeof entry.modified === 'number' && isFinite(entry.modified) ? entry.modified : 0;
  }

  function compareEntries(a, b) {
    // 文件夹始终排在文件前面，与升降序无关。
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;

    var direction = state.sortOrder === 'desc' ? -1 : 1;
    var result;

    if (state.sortKey === 'size') result = a.size - b.size;
    else if (state.sortKey === 'modified') result = modifiedValue(a) - modifiedValue(b);
    else if (state.sortKey === 'ext') result = compareName(a.ext, b.ext);
    else result = compareName(a.name, b.name);

    if (result === 0) result = compareName(a.name, b.name);
    return result * direction;
  }

  /* ------------------------------------------------------------------------
   * Toast 提示
   * ---------------------------------------------------------------------- */

  function toast(message, type) {
    if (!message) return;
    var kind = type || 'info';
    var item = document.createElement('div');
    item.className = 'toast toast-' + kind;

    var text = document.createElement('span');
    text.className = 'toast-text';
    text.textContent = message;

    var close = document.createElement('button');
    close.type = 'button';
    close.className = 'toast-close';
    close.setAttribute('aria-label', '关闭提示');
    close.textContent = '✕';

    item.appendChild(text);
    item.appendChild(close);
    el.toastRoot.appendChild(item);

    var timer = window.setTimeout(remove, kind === 'error' ? 7000 : 4000);

    function remove() {
      window.clearTimeout(timer);
      if (item.parentNode) item.parentNode.removeChild(item);
    }

    close.addEventListener('click', remove);

    while (el.toastRoot.children.length > 5) {
      el.toastRoot.removeChild(el.toastRoot.firstChild);
    }
  }

  /* ------------------------------------------------------------------------
   * 顶栏 / 只读状态
   * ---------------------------------------------------------------------- */

  function updateWritableUi() {
    if (state.lastError) {
      el.modeBadge.className = 'badge badge-error';
      el.modeBadge.textContent = '加载失败';
      el.writeActions.hidden = true;
      el.readonlyBar.hidden = true;
      return;
    }

    if (!state.dataReady) {
      el.modeBadge.className = 'badge badge-loading';
      el.modeBadge.textContent = '加载中';
      el.writeActions.hidden = true;
      el.readonlyBar.hidden = true;
      return;
    }

    var writable = state.writable === true;
    el.modeBadge.className = 'badge ' + (writable ? 'badge-writable' : 'badge-readonly');
    el.modeBadge.textContent = writable ? '可写模式' : '只读模式';
    el.writeActions.hidden = !writable;
    el.readonlyBar.hidden = writable;
    el.uploadFileBtn.disabled = state.uploading;
    el.uploadDirBtn.disabled = state.uploading;
    el.newFolderBtn.disabled = state.uploading;
    el.dropHint.textContent = writable ? '支持多选文件与整个文件夹' : '只读模式：无法上传';
  }

  function updateBrandSub() {
    if (state.lastError) {
      el.brandSub.textContent = '加载失败：' + (state.lastError.message || '未知错误');
      return;
    }
    if (!state.dataReady) {
      el.brandSub.textContent = '正在加载 ' + state.dir + ' …';
      return;
    }
    el.brandSub.textContent = '当前位置：' + state.dir;
  }

  function titleForDir() {
    var segments = splitDir(state.dir);
    return (segments.length ? segments[segments.length - 1] : '根目录') + ' · 文件服务器';
  }

  /* ------------------------------------------------------------------------
   * 加载目录
   * ---------------------------------------------------------------------- */

  function loadDirectory() {
    var token = state.loadToken + 1;
    state.loadToken = token;
    state.loading = true;
    state.dataReady = false;
    state.lastError = null;
    state.entries = [];
    el.contentCard.setAttribute('aria-busy', 'true');
    render();

    window.fetch(API.list + '?dir=' + encodeURIComponent(state.dir), {
      method: 'GET',
      cache: 'no-store',
      headers: { 'Accept': 'application/json' }
    }).then(function (response) {
      return response.text().then(function (text) {
        var payload = parseJsonSafe(text);
        if (!response.ok) throw makeHttpError(response.status, payload);
        if (!payload || payload.ok !== true) {
          throw makeHttpError(response.status, payload, '返回数据格式不正确');
        }
        return payload;
      });
    }).then(function (payload) {
      if (token !== state.loadToken) return;
      state.loading = false;
      state.dataReady = true;
      state.writable = payload.writable === true;
      var rawEntries = Array.isArray(payload.entries) ? payload.entries : [];
      state.entries = rawEntries.map(normalizeEntry).filter(Boolean);
      document.title = titleForDir();
      el.contentCard.setAttribute('aria-busy', 'false');
      render();
    }).catch(function (error) {
      if (token !== state.loadToken) return;
      state.loading = false;
      state.dataReady = false;
      state.lastError = error instanceof Error ? error : new Error(describeException(error));
      el.contentCard.setAttribute('aria-busy', 'false');
      render();
      toast(state.lastError.message || '加载目录失败', 'error');
    });
  }

  /* ------------------------------------------------------------------------
   * 渲染：总入口
   * ---------------------------------------------------------------------- */

  function render() {
    updateWritableUi();
    updateBrandSub();
    renderBreadcrumb();
    renderUpButton();
    renderSortBar();
    renderViewToggle();
    renderSidebar();
    renderContent();
    renderStats();
  }

  function renderBreadcrumb() {
    var nav = el.breadcrumb;
    nav.textContent = '';
    var segments = splitDir(state.dir);

    var root = document.createElement('a');
    root.className = 'crumb' + (segments.length === 0 ? ' is-current' : '');
    root.href = '/';
    root.title = '根目录';
    root.textContent = '🏠 根目录';
    if (segments.length === 0) root.setAttribute('aria-current', 'page');
    root.addEventListener('click', (function (targetSegments) {
      return function (event) { onCrumbClick(event, targetSegments); };
    })([]));
    nav.appendChild(root);

    segments.forEach(function (segment, index) {
      var separator = document.createElement('span');
      separator.className = 'crumb-sep';
      separator.setAttribute('aria-hidden', 'true');
      separator.textContent = '/';
      nav.appendChild(separator);

      var partial = segments.slice(0, index + 1);
      var link = document.createElement('a');
      link.className = 'crumb' + (index === segments.length - 1 ? ' is-current' : '');
      link.href = dirUrl(partial);
      link.title = segment;
      link.textContent = segment;
      if (index === segments.length - 1) link.setAttribute('aria-current', 'page');
      link.addEventListener('click', (function (targetSegments) {
        return function (event) { onCrumbClick(event, targetSegments); };
      })(partial));
      nav.appendChild(link);
    });
  }

  function renderUpButton() {
    var segments = splitDir(state.dir);
    if (segments.length === 0) {
      el.upBtn.disabled = true;
      el.upBtn.title = '已经是根目录';
    } else {
      el.upBtn.disabled = false;
      el.upBtn.title = '返回上一级';
    }
  }

  function renderSortBar() {
    var buttons = el.sortBar.querySelectorAll('[data-sort]');
    for (var index = 0; index < buttons.length; index += 1) {
      var button = buttons[index];
      var key = button.getAttribute('data-sort');
      var active = key === state.sortKey;
      var arrow = button.querySelector('.sort-arrow');
      button.classList.toggle('is-active', active);
      button.setAttribute('aria-pressed', active ? 'true' : 'false');
      if (arrow) arrow.textContent = active ? (state.sortOrder === 'asc' ? '▲' : '▼') : '';
      button.title = active
        ? (state.sortOrder === 'asc' ? '当前升序，点击切换为降序' : '当前降序，点击切换为升序')
        : '按' + (SORT_LABELS[key] || key) + '排序';
    }
  }

  function renderViewToggle() {
    var grouped = state.view === 'grouped';
    el.groupedViewBtn.classList.toggle('is-active', grouped);
    el.groupedViewBtn.setAttribute('aria-pressed', grouped ? 'true' : 'false');
    el.flatViewBtn.classList.toggle('is-active', !grouped);
    el.flatViewBtn.setAttribute('aria-pressed', grouped ? 'false' : 'true');
  }

  function renderSidebar() {
    el.catNav.textContent = '';

    if (!state.dataReady) {
      var note = document.createElement('p');
      note.className = 'cat-empty';
      note.textContent = state.lastError ? '目录加载失败' : '正在加载…';
      el.catNav.appendChild(note);
      el.navTotal.textContent = state.lastError ? '加载失败' : '加载中…';
      return;
    }

    var counts = Object.create(null);
    state.entries.forEach(function (entry) {
      var category = categoryOf(entry);
      counts[category] = (counts[category] || 0) + 1;
    });

    el.navTotal.textContent = '共 ' + state.entries.length + ' 项';

    var rendered = 0;
    CATEGORY_DEFS.forEach(function (category) {
      var count = counts[category.id] || 0;
      if (!count) return;
      rendered += 1;

      var button = document.createElement('button');
      button.type = 'button';
      button.className = 'cat-btn';
      button.setAttribute('data-category', category.id);
      button.title = '跳转到「' + category.name + '」分类（' + count + ' 项）';

      var icon = document.createElement('span');
      icon.className = 'cat-icon';
      icon.setAttribute('aria-hidden', 'true');
      icon.textContent = category.icon;

      var name = document.createElement('span');
      name.className = 'cat-name';
      name.textContent = category.name;

      var countEl = document.createElement('span');
      countEl.className = 'cat-count';
      countEl.textContent = String(count);

      button.appendChild(icon);
      button.appendChild(name);
      button.appendChild(countEl);
      el.catNav.appendChild(button);
    });

    if (!rendered) {
      var empty = document.createElement('p');
      empty.className = 'cat-empty';
      empty.textContent = state.entries.length ? '暂无可分类内容' : '当前文件夹为空';
      el.catNav.appendChild(empty);
    }
  }

  function renderStats() {
    if (state.lastError) {
      el.statsLine.textContent = '';
      return;
    }
    if (!state.dataReady) {
      el.statsLine.textContent = '正在加载…';
      return;
    }

    var folders = 0;
    var files = 0;
    var bytes = 0;
    state.entries.forEach(function (entry) {
      if (entry.isDir) {
        folders += 1;
      } else {
        files += 1;
        bytes += entry.size;
      }
    });

    var text = '共 ' + state.entries.length + ' 项 · ' + folders + ' 个文件夹 · ' + files + ' 个文件';
    if (files > 0) text += ' · 合计 ' + formatSize(bytes);
    el.statsLine.textContent = text;
  }

  function renderContent() {
    var card = el.contentCard;
    card.textContent = '';

    if (state.lastError) {
      renderErrorState(card);
      return;
    }
    if (!state.dataReady) {
      renderSkeleton(card);
      return;
    }
    if (state.entries.length === 0) {
      renderEmptyState(card);
      return;
    }
    if (state.view === 'flat') renderFlat(card);
    else renderGrouped(card);
  }

  /* ------------------------------------------------------------------------
   * 渲染：加载 / 空 / 错误状态
   * ---------------------------------------------------------------------- */

  function renderSkeleton(card) {
    var wrap = document.createElement('div');
    wrap.className = 'skeleton';
    for (var index = 0; index < 7; index += 1) {
      var row = document.createElement('div');
      row.className = 'skeleton-row' + (index === 0 ? ' skeleton-head' : '');
      wrap.appendChild(row);
    }
    card.appendChild(wrap);
  }

  function renderErrorState(card) {
    var wrap = document.createElement('div');
    wrap.className = 'state state-error';

    var icon = document.createElement('div');
    icon.className = 'state-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = '⚠️';

    var title = document.createElement('h2');
    title.className = 'state-title';
    title.textContent = '无法加载文件列表';

    var message = document.createElement('p');
    message.className = 'state-text';
    message.textContent = state.lastError && state.lastError.message ? state.lastError.message : '未知错误';

    var actions = document.createElement('div');
    actions.className = 'state-actions';
    var retry = document.createElement('button');
    retry.type = 'button';
    retry.className = 'btn btn-primary';
    retry.textContent = '重试';
    retry.addEventListener('click', function () { loadDirectory(); });
    actions.appendChild(retry);

    wrap.appendChild(icon);
    wrap.appendChild(title);
    wrap.appendChild(message);
    wrap.appendChild(actions);
    card.appendChild(wrap);
  }

  function renderEmptyState(card) {
    var wrap = document.createElement('div');
    wrap.className = 'state';

    var icon = document.createElement('div');
    icon.className = 'state-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = '📂';

    var title = document.createElement('h2');
    title.className = 'state-title';
    title.textContent = '这个文件夹是空的';

    var text = document.createElement('p');
    text.className = 'state-text';
    text.textContent = state.writable
      ? '点击右上角「上传文件」、把文件拖到页面中，或新建一个文件夹。'
      : '当前目录没有可显示的内容。';

    wrap.appendChild(icon);
    wrap.appendChild(title);
    wrap.appendChild(text);

    if (state.writable) {
      var actions = document.createElement('div');
      actions.className = 'state-actions';

      var upload = document.createElement('button');
      upload.type = 'button';
      upload.className = 'btn btn-primary';
      upload.textContent = '⬆ 上传文件';
      upload.addEventListener('click', triggerFilePicker);
      actions.appendChild(upload);

      var folder = document.createElement('button');
      folder.type = 'button';
      folder.className = 'btn btn-secondary';
      folder.textContent = '＋ 新建文件夹';
      folder.addEventListener('click', openMkdirModal);
      actions.appendChild(folder);

      wrap.appendChild(actions);
    }

    card.appendChild(wrap);
  }

  /* ------------------------------------------------------------------------
   * 渲染：分类分组 / 平铺列表 / 条目行
   * ---------------------------------------------------------------------- */

  function groupEntries() {
    var groups = CATEGORY_DEFS.map(function (category) {
      return { meta: category, entries: [] };
    });
    var byId = Object.create(null);
    groups.forEach(function (group) { byId[group.meta.id] = group; });

    state.entries.slice().sort(compareEntries).forEach(function (entry) {
      var group = byId[categoryOf(entry)] || byId.other;
      group.entries.push(entry);
    });

    return groups.filter(function (group) { return group.entries.length > 0; });
  }

  function renderGrouped(card) {
    groupEntries().forEach(function (group) {
      card.appendChild(buildGroup(group));
    });
  }

  function buildGroup(group) {
    var meta = group.meta;
    var collapsed = state.collapsed[meta.id] === true;

    var section = document.createElement('section');
    section.className = 'group' + (collapsed ? ' is-collapsed' : '');
    section.setAttribute('data-group', meta.id);

    var header = document.createElement('div');
    header.className = 'group-header';

    var toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'group-toggle';
    toggle.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    toggle.setAttribute('aria-controls', 'group-body-' + meta.id);

    var chevron = document.createElement('span');
    chevron.className = 'group-chevron';
    chevron.setAttribute('aria-hidden', 'true');
    chevron.textContent = '▾';

    var icon = document.createElement('span');
    icon.className = 'group-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = meta.icon;

    var name = document.createElement('span');
    name.className = 'group-name';
    name.textContent = meta.name;

    var count = document.createElement('span');
    count.className = 'group-count';
    count.textContent = group.entries.length + ' 项';

    toggle.appendChild(chevron);
    toggle.appendChild(icon);
    toggle.appendChild(name);
    toggle.appendChild(count);
    header.appendChild(toggle);
    section.appendChild(header);

    var body = document.createElement('div');
    body.className = 'group-body';
    body.id = 'group-body-' + meta.id;
    if (collapsed) body.hidden = true;

    group.entries.forEach(function (entry) {
      body.appendChild(buildEntryRow(entry));
    });

    section.appendChild(body);
    return section;
  }

  function renderFlat(card) {
    var wrap = document.createElement('div');
    wrap.className = 'flat-list';

    var head = document.createElement('div');
    head.className = 'entry-row entry-head';
    head.appendChild(buildHeadCell('name', '名称', 'cell-name'));
    head.appendChild(buildHeadCell('size', '大小', 'cell-size'));
    head.appendChild(buildHeadCell('modified', '修改时间', 'cell-time'));
    head.appendChild(buildHeadCell('ext', '类型', 'cell-type'));
    wrap.appendChild(head);

    state.entries.slice().sort(compareEntries).forEach(function (entry) {
      wrap.appendChild(buildEntryRow(entry));
    });

    card.appendChild(wrap);
  }

  function buildHeadCell(key, label, extraClass) {
    var cell = document.createElement('div');
    cell.className = 'cell ' + extraClass;

    var button = document.createElement('button');
    button.type = 'button';
    button.className = 'col-sort' + (state.sortKey === key ? ' is-active' : '');
    button.setAttribute('data-sort', key);
    button.title = '按' + label + '排序';
    button.textContent = label;

    var arrow = document.createElement('span');
    arrow.className = 'sort-arrow';
    arrow.setAttribute('aria-hidden', 'true');
    if (state.sortKey === key) arrow.textContent = state.sortOrder === 'asc' ? '▲' : '▼';
    button.appendChild(arrow);

    button.addEventListener('click', function () { setSort(key); });
    cell.appendChild(button);
    return cell;
  }

  function buildEntryRow(entry) {
    var row = document.createElement('div');
    row.className = 'entry-row ' + (entry.isDir ? 'is-dir' : 'is-file');

    var nameCell = document.createElement('div');
    nameCell.className = 'cell cell-name';

    var icon = document.createElement('span');
    icon.className = 'entry-icon';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = entry.isDir ? '📁' : categoryMeta(categoryOf(entry)).icon;

    var link = document.createElement('a');
    link.className = 'entry-link';
    link.textContent = entry.name;
    link.title = entry.name;

    if (entry.isDir) {
      var segments = segmentsFromEncodedPath(entry.path);
      link.href = dirUrl(segments);
      link.setAttribute('aria-label', '打开文件夹 ' + entry.name);
      link.addEventListener('click', (function (targetSegments) {
        return function (event) { onDirectoryClick(event, targetSegments); };
      })(segments));
    } else {
      link.href = entry.path || '#';
      link.setAttribute('aria-label', '下载文件 ' + entry.name);
    }

    nameCell.appendChild(icon);
    nameCell.appendChild(link);

    var sizeCell = document.createElement('div');
    sizeCell.className = 'cell cell-size';
    sizeCell.textContent = entry.isDir ? '—' : formatSize(entry.size);

    var timeCell = document.createElement('div');
    timeCell.className = 'cell cell-time';
    timeCell.textContent = formatTime(entry.modified);

    var typeCell = document.createElement('div');
    typeCell.className = 'cell cell-type';
    typeCell.textContent = entry.isDir ? '文件夹' : (entry.ext ? entry.ext.toUpperCase() : '—');

    row.appendChild(nameCell);
    row.appendChild(sizeCell);
    row.appendChild(timeCell);
    row.appendChild(typeCell);
    return row;
  }

  /* ------------------------------------------------------------------------
   * 排序 / 视图切换 / 分类跳转 / 分组折叠
   * ---------------------------------------------------------------------- */

  function setSort(key) {
    if (SORT_KEYS.indexOf(key) < 0) return;
    if (state.sortKey === key) {
      state.sortOrder = state.sortOrder === 'asc' ? 'desc' : 'asc';
    } else {
      state.sortKey = key;
      state.sortOrder = 'asc';
    }
    renderSortBar();
    renderContent();
  }

  function setView(view) {
    if (view !== 'grouped' && view !== 'flat') return;
    if (state.view === view) return;
    state.view = view;
    renderViewToggle();
    renderContent();
    renderSidebar();
  }

  function jumpToCategory(categoryId) {
    if (state.view !== 'grouped') setView('grouped');

    var buttons = el.catNav.querySelectorAll('.cat-btn');
    for (var index = 0; index < buttons.length; index += 1) {
      buttons[index].classList.toggle('is-active', buttons[index].getAttribute('data-category') === categoryId);
    }

    var target = el.contentCard.querySelector('[data-group="' + categoryId + '"]');
    if (!target) return;

    try {
      var reduceMotion = typeof window.matchMedia === 'function' &&
        window.matchMedia('(prefers-reduced-motion: reduce)').matches;
      target.scrollIntoView({ behavior: reduceMotion ? 'auto' : 'smooth', block: 'start' });
    } catch (error) {
      try { target.scrollIntoView(); } catch (ignored) { /* 忽略 */ }
    }

    target.classList.add('is-flash');
    window.setTimeout(function () { target.classList.remove('is-flash'); }, 1200);
  }

  function onContentClick(event) {
    var target = event.target;
    var toggle = target && target.closest ? target.closest('.group-toggle') : null;
    if (!toggle) return;

    var section = toggle.closest('.group');
    if (!section) return;

    var groupId = section.getAttribute('data-group') || '';
    var body = section.querySelector('.group-body');
    var collapsed = !section.classList.contains('is-collapsed');

    section.classList.toggle('is-collapsed', collapsed);
    toggle.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    if (body) body.hidden = collapsed;
    if (groupId) state.collapsed[groupId] = collapsed;
  }

  /* ------------------------------------------------------------------------
   * 导航
   * ---------------------------------------------------------------------- */

  function navigateTo(dir, url) {
    var targetDir = dir || '/';
    if (targetDir === state.dir) {
      loadDirectory();
      return;
    }
    try {
      window.history.pushState({ dir: targetDir }, '', url);
    } catch (error) {
      window.location.href = url;
      return;
    }
    state.dir = targetDir;
    state.collapsed = Object.create(null);
    loadDirectory();
  }

  function onCrumbClick(event, segments) {
    if (event.defaultPrevented) return;
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    navigateTo(joinDir(segments), dirUrl(segments));
  }

  function onDirectoryClick(event, segments) {
    if (event.defaultPrevented) return;
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    navigateTo(joinDir(segments), dirUrl(segments));
  }

  function onUpClick() {
    var segments = splitDir(state.dir);
    if (!segments.length) return;
    segments.pop();
    navigateTo(joinDir(segments), dirUrl(segments));
  }

  function onPopState() {
    state.dir = dirFromPathname(window.location.pathname);
    state.collapsed = Object.create(null);
    loadDirectory();
  }

  /* ------------------------------------------------------------------------
   * 上传：入口 / 选择 / 拖放收集
   * ---------------------------------------------------------------------- */

  function ensureWritable(actionLabel) {
    if (state.writable) return true;
    toast('服务器为只读模式，无法' + (actionLabel || '执行该操作'), 'error');
    return false;
  }

  function triggerFilePicker() {
    if (!ensureWritable('上传文件')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }
    el.fileInput.click();
  }

  function triggerDirPicker() {
    if (!ensureWritable('上传文件夹')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }
    el.dirInput.click();
  }

  function normalizeUploadPath(path, fallbackName) {
    var value = typeof path === 'string' && path ? path : (fallbackName || '');
    value = value.replace(/\\/g, '/').replace(/^\/+/, '');
    return value || (fallbackName || '');
  }

  function onFileInputChange() {
    var files = sliceFileList(el.fileInput.files);
    el.fileInput.value = '';
    if (!files.length) return;
    if (!ensureWritable('上传文件')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }
    startUpload(files.map(function (file) {
      return { file: file, path: file.name };
    }));
  }

  function onDirInputChange() {
    var files = sliceFileList(el.dirInput.files);
    el.dirInput.value = '';
    if (!files.length) return;
    if (!ensureWritable('上传文件夹')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }
    startUpload(files.map(function (file) {
      var relative = '';
      try {
        relative = file.webkitRelativePath || '';
      } catch (error) {
        relative = '';
      }
      return { file: file, path: relative || file.name };
    }));
  }

  function hasFilePayload(event) {
    var transfer = event.dataTransfer;
    if (!transfer) return false;
    if (transfer.types) {
      for (var index = 0; index < transfer.types.length; index += 1) {
        if (transfer.types[index] === 'Files') return true;
      }
    }
    return !!(transfer.files && transfer.files.length > 0);
  }

  function hideDropOverlay() {
    dragDepth = 0;
    el.dropOverlay.hidden = true;
  }

  function showDropOverlay() {
    el.dropHint.textContent = state.writable ? '支持多选文件与整个文件夹' : '只读模式：无法上传';
    el.dropOverlay.classList.toggle('is-disabled', !state.writable);
    el.dropOverlay.hidden = false;
  }

  function onDragEnter(event) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    dragDepth += 1;
    showDropOverlay();
  }

  function onDragOver(event) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    if (event.dataTransfer) {
      try { event.dataTransfer.dropEffect = 'copy'; } catch (error) { /* 忽略 */ }
    }
    if (el.dropOverlay.hidden) showDropOverlay();
  }

  function onDragLeave(event) {
    if (!hasFilePayload(event)) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) hideDropOverlay();
  }

  function onDrop(event) {
    if (!hasFilePayload(event)) return;
    event.preventDefault();
    hideDropOverlay();

    if (!ensureWritable('上传文件')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }

    var transfer = event.dataTransfer;
    if (!transfer) return;

    collectDroppedItems(transfer).then(function (items) {
      if (!items.length) {
        toast('未检测到可上传的文件', 'info');
        return;
      }
      startUpload(items);
    }).catch(function (error) {
      toast('读取拖入的文件失败：' + describeException(error), 'error');
    });
  }

  function collectDroppedItems(transfer) {
    var items = transfer ? transfer.items : null;
    var roots = [];

    if (items && items.length) {
      for (var index = 0; index < items.length; index += 1) {
        var item = items[index];
        if (!item || item.kind !== 'file') continue;
        var entry = null;
        try {
          entry = item.webkitGetAsEntry ? item.webkitGetAsEntry() : null;
        } catch (error) {
          entry = null;
        }
        if (entry) roots.push(entry);
      }
    }

    if (!roots.length) {
      var files = sliceFileList(transfer ? transfer.files : null);
      return Promise.resolve(files.map(function (file) {
        return { file: file, path: file.name };
      }));
    }

    return collectEntryRoots(roots);
  }

  function collectEntryRoots(roots) {
    var collected = [];
    var chain = Promise.resolve();
    roots.forEach(function (root) {
      chain = chain.then(function () {
        return collectEntry(root, '').then(function (items) {
          collected = collected.concat(items);
        });
      });
    });
    return chain.then(function () { return collected; });
  }

  function collectEntry(entry, prefix) {
    if (!entry) return Promise.resolve([]);

    if (entry.isFile) {
      return new Promise(function (resolve) {
        try {
          entry.file(function (file) {
            resolve([{ file: file, path: prefix + entry.name }]);
          }, function () {
            resolve([]);
          });
        } catch (error) {
          resolve([]);
        }
      });
    }

    if (entry.isDirectory) {
      var dirPrefix = prefix + entry.name + '/';
      var reader;
      try {
        reader = entry.createReader();
      } catch (error) {
        return Promise.resolve([]);
      }
      return readAllDirectoryEntries(reader).then(function (children) {
        return collectEntryChildren(children, dirPrefix);
      });
    }

    return Promise.resolve([]);
  }

  function collectEntryChildren(children, prefix) {
    var collected = [];
    var chain = Promise.resolve();
    children.forEach(function (child) {
      chain = chain.then(function () {
        return collectEntry(child, prefix).then(function (items) {
          collected = collected.concat(items);
        });
      });
    });
    return chain.then(function () { return collected; });
  }

  // readEntries 一次可能只返回部分条目，必须循环读到空数组为止。
  function readAllDirectoryEntries(reader) {
    return new Promise(function (resolve) {
      var all = [];
      var readBatch = function () {
        reader.readEntries(function (batch) {
          if (!batch || !batch.length) {
            resolve(all);
            return;
          }
          for (var index = 0; index < batch.length; index += 1) all.push(batch[index]);
          readBatch();
        }, function () {
          resolve(all);
        });
      };
      readBatch();
    });
  }

  /* ------------------------------------------------------------------------
   * 上传：逐个文件 XHR + 累计进度
   * ---------------------------------------------------------------------- */

  function setUploadingUi(active) {
    state.uploading = active;
    if (el.fileInput) el.fileInput.disabled = active;
    if (el.dirInput) el.dirInput.disabled = active;
    if (el.transferCloseBtn) {
      el.transferCloseBtn.title = active ? '取消上传' : '关闭';
      el.transferCloseBtn.setAttribute('aria-label', active ? '取消上传' : '关闭上传面板');
    }
    updateWritableUi();
  }

  function showTransferPanel() {
    el.transferPanel.hidden = false;
  }

  function hideTransferPanel() {
    el.transferPanel.hidden = true;
  }

  function startUpload(items) {
    if (!items || !items.length) {
      toast('没有可上传的文件', 'info');
      return;
    }
    if (!ensureWritable('上传文件')) return;
    if (state.uploading) {
      toast('已有上传任务正在进行，请稍候', 'info');
      return;
    }

    var prepared = [];
    for (var index = 0; index < items.length; index += 1) {
      var item = items[index];
      if (!item || !item.file) continue;
      var relative = normalizeUploadPath(item.path, item.file.name);
      if (!relative) continue;
      prepared.push({ file: item.file, path: relative });
    }
    if (!prepared.length) {
      toast('没有可上传的文件', 'info');
      return;
    }

    var totalBytes = 0;
    prepared.forEach(function (entry) { totalBytes += fileSize(entry.file); });

    uploadSerial += 1;
    uploadSession = {
      id: uploadSerial,
      items: prepared,
      index: 0,
      totalBytes: totalBytes,
      completedBytes: 0,
      loadedBytes: 0,
      saved: 0,
      failed: [],
      dir: state.dir,
      xhr: null,
      active: true,
      stopNote: ''
    };

    setUploadingUi(true);
    showTransferPanel();
    el.progressFill.classList.remove('is-success', 'is-error');
    el.progressFill.style.width = '0%';
    el.progressBar.setAttribute('aria-valuenow', '0');
    el.transferBody.textContent = '';
    el.transferBody.hidden = true;
    updateUploadProgress(uploadSession);
    uploadNextFile();
  }

  function uploadNextFile() {
    var session = uploadSession;
    if (!session || !session.active) return;
    if (session.index >= session.items.length) {
      finishUploadSession();
      return;
    }

    var item = session.items[session.index];
    var file = item.file;
    var xhr = new XMLHttpRequest();
    session.xhr = xhr;

    el.transferTitle.textContent = '正在上传（' + (session.index + 1) + ' / ' + session.items.length + '）';
    showTransferCurrentFile(item);

    xhr.open(
      'POST',
      API.upload + '?dir=' + encodeURIComponent(session.dir) + '&path=' + encodeURIComponent(item.path),
      true
    );
    xhr.setRequestHeader('Content-Type', 'application/octet-stream');

    xhr.upload.onprogress = function (event) {
      if (!session.active) return;
      var inFile = event.lengthComputable ? event.loaded : (session.loadedBytes - session.completedBytes);
      if (inFile < 0) inFile = 0;
      session.loadedBytes = Math.min(session.totalBytes, session.completedBytes + inFile);
      updateUploadProgress(session);
    };

    xhr.onload = function () {
      if (!session.active) return;
      session.xhr = null;
      var payload = parseJsonSafe(xhr.responseText);
      var success = xhr.status >= 200 && xhr.status < 300 && payload && payload.ok !== false;
      if (success) {
        markUploadFileDone(session, '');
      } else {
        var message = payload && typeof payload.error === 'string' && payload.error
          ? payload.error
          : describeStatus(xhr.status);
        markUploadFileDone(session, message);
        if (xhr.status === 403) session.stopNote = message;
      }
      afterUploadStep(session);
    };

    xhr.onerror = function () {
      if (!session.active) return;
      session.xhr = null;
      markUploadFileDone(session, '网络错误，上传失败');
      afterUploadStep(session);
    };

    xhr.ontimeout = function () {
      if (!session.active) return;
      session.xhr = null;
      markUploadFileDone(session, '上传超时，请稍后重试');
      afterUploadStep(session);
    };

    // 取消上传统一由 cancelUpload 处理，这里不做额外动作。
    xhr.onabort = function () {};

    try {
      xhr.send(file);
    } catch (error) {
      if (!session.active) return;
      session.xhr = null;
      markUploadFileDone(session, '无法发起上传：' + describeException(error));
      afterUploadStep(session);
    }
  }

  function showTransferCurrentFile(item) {
    el.transferBody.textContent = '';
    el.transferBody.hidden = false;
    var line = document.createElement('p');
    line.className = 'transfer-summary';
    line.textContent = '正在上传：' + item.path;
    line.title = item.path;
    el.transferBody.appendChild(line);
  }

  function markUploadFileDone(session, errorMessage) {
    var item = session.items[session.index];
    if (!item) return;
    if (errorMessage) session.failed.push({ path: item.path, error: errorMessage });
    else session.saved += 1;
    session.completedBytes = Math.min(session.totalBytes, session.completedBytes + fileSize(item.file));
    session.loadedBytes = session.completedBytes;
    session.index += 1;
  }

  function afterUploadStep(session) {
    if (!session || !session.active) return;
    updateUploadProgress(session);
    if (session.stopNote) {
      finishUploadSession();
      return;
    }
    uploadNextFile();
  }

  function updateUploadProgress(session) {
    if (!session) return;
    var percent = 0;
    if (session.totalBytes > 0) {
      percent = Math.min(100, (session.loadedBytes / session.totalBytes) * 100);
    } else if (session.items.length) {
      percent = Math.min(100, (session.index / session.items.length) * 100);
    }
    el.progressFill.style.width = percent.toFixed(1) + '%';
    el.progressBar.setAttribute('aria-valuenow', String(Math.round(percent)));
    el.progressText.textContent = Math.round(percent) + '%';
    el.progressDetail.textContent = formatSize(session.loadedBytes) + ' / ' + formatSize(session.totalBytes);
  }

  function finishUploadSession() {
    var session = uploadSession;
    if (!session) return;

    session.active = false;
    session.loadedBytes = session.totalBytes;
    var skipped = Math.max(0, session.items.length - session.index);
    session.xhr = null;
    uploadSession = null;

    setUploadingUi(false);
    renderUploadResult(session, skipped);

    var summary;
    if (session.stopNote) {
      summary = session.stopNote;
    } else if (session.saved > 0) {
      summary = '上传完成：成功 ' + session.saved + ' 个文件' +
        (session.failed.length ? '，失败 ' + session.failed.length + ' 个' : '');
    } else {
      summary = '上传失败：没有文件保存成功';
    }
    var kind = session.saved > 0 ? (session.failed.length ? 'warn' : 'success') : 'error';
    toast(summary, kind);

    if (state.dir === session.dir) loadDirectory();
  }

  function renderUploadResult(session, skipped) {
    var total = session.items.length;
    var allFailed = session.saved === 0;

    el.transferPanel.hidden = false;
    el.transferTitle.textContent = allFailed ? '上传失败' : (session.failed.length ? '上传部分完成' : '上传完成');
    el.progressFill.style.width = '100%';
    el.progressFill.classList.remove('is-success', 'is-error');
    el.progressFill.classList.add(allFailed ? 'is-error' : 'is-success');
    el.progressBar.setAttribute('aria-valuenow', '100');
    el.progressText.textContent = '100%';
    el.progressDetail.textContent = '成功 ' + session.saved + ' / ' + total + ' 个文件';

    el.transferBody.textContent = '';
    el.transferBody.hidden = false;

    var summary = document.createElement('p');
    summary.className = 'transfer-summary';
    var summaryText = '成功保存 ' + session.saved + ' 个文件';
    if (session.failed.length) summaryText += '，失败 ' + session.failed.length + ' 个';
    if (skipped > 0) summaryText += '，其余 ' + skipped + ' 个未上传';
    summary.textContent = summaryText + '。';
    el.transferBody.appendChild(summary);

    if (session.failed.length) {
      var list = document.createElement('ul');
      list.className = 'fail-list';

      session.failed.slice(0, 5).forEach(function (failure) {
        var item = document.createElement('li');
        item.className = 'fail-item';

        var fullPath = typeof failure.path === 'string' && failure.path ? failure.path : '（未知文件）';
        var name = document.createElement('span');
        name.className = 'fail-path';
        name.textContent = truncateText(fullPath, 120);
        name.title = fullPath;

        var fullReason = typeof failure.error === 'string' && failure.error ? failure.error : '保存失败';
        var reason = document.createElement('span');
        reason.className = 'fail-reason';
        reason.textContent = truncateText(fullReason, 80);
        reason.title = fullReason;

        item.appendChild(name);
        item.appendChild(reason);
        list.appendChild(item);
      });

      if (session.failed.length > 5) {
        var more = document.createElement('li');
        more.className = 'fail-more';
        more.textContent = '另有 ' + (session.failed.length - 5) + ' 个失败项未显示';
        list.appendChild(more);
      }

      el.transferBody.appendChild(list);
    }
  }

  function cancelUpload() {
    var session = uploadSession;
    if (!session) return;

    session.active = false;
    var xhr = session.xhr;
    session.xhr = null;
    uploadSession = null;

    if (xhr) {
      try { xhr.abort(); } catch (error) { /* 忽略 */ }
    }

    setUploadingUi(false);
    hideTransferPanel();

    if (session.saved > 0) {
      toast('已取消上传，已保存 ' + session.saved + ' 个文件', 'info');
      if (state.dir === session.dir) loadDirectory();
    } else {
      toast('已取消上传', 'info');
    }
  }

  function onTransferClose() {
    if (state.uploading && uploadSession) {
      cancelUpload();
      return;
    }
    hideTransferPanel();
  }

  /* ------------------------------------------------------------------------
   * 新建文件夹（页面内模态框）
   * ---------------------------------------------------------------------- */

  function validateFolderName(name) {
    if (!name) return '请输入文件夹名称';
    if (name.length > 255) return '文件夹名称过长（最多 255 个字符）';
    if (name === '.' || name === '..') return '文件夹名称不能是 . 或 ..';
    if (name.indexOf('/') >= 0 || name.indexOf('\\') >= 0 || name.indexOf(':') >= 0) {
      return '文件夹名称不能包含 / \\ : 等字符';
    }
    if (/[\u0000-\u001f\u007f]/.test(name)) return '文件夹名称不能包含控制字符';
    if (/[. ]$/.test(name)) return '文件夹名称不能以点或空格结尾';
    return '';
  }

  function showMkdirError(message) {
    if (!message) {
      el.mkError.textContent = '';
      el.mkError.hidden = true;
      return;
    }
    el.mkError.textContent = message;
    el.mkError.hidden = false;
  }

  function setMkdirPending(pending) {
    mkdirState.pending = pending;
    el.mkSubmitBtn.disabled = pending;
    el.mkSubmitBtn.textContent = pending ? '创建中…' : '创建';
    el.mkCancelBtn.disabled = pending;
    el.mkCloseBtn.disabled = pending;
    el.mkName.disabled = pending;
  }

  function openMkdirModal() {
    if (!ensureWritable('新建文件夹')) return;
    mkdirState.focusBack = document.activeElement;
    setMkdirPending(false);
    el.mkName.value = '';
    showMkdirError('');
    el.modalBackdrop.hidden = false;
    document.body.classList.add('modal-open');
    window.setTimeout(function () {
      try {
        el.mkName.focus();
        el.mkName.select();
      } catch (error) { /* 忽略 */ }
    }, 30);
  }

  function closeMkdirModal(force) {
    if (mkdirState.pending && !force) return;
    el.modalBackdrop.hidden = true;
    document.body.classList.remove('modal-open');
    if (mkdirState.focusBack && typeof mkdirState.focusBack.focus === 'function') {
      try { mkdirState.focusBack.focus(); } catch (error) { /* 忽略 */ }
    }
    mkdirState.focusBack = null;
  }

  function submitMkdir() {
    if (mkdirState.pending) return;

    var name = el.mkName.value.trim();
    var problem = validateFolderName(name);
    if (problem) {
      showMkdirError(problem);
      el.mkName.focus();
      return;
    }

    setMkdirPending(true);
    showMkdirError('');

    window.fetch(API.mkdir + '?dir=' + encodeURIComponent(state.dir) + '&name=' + encodeURIComponent(name), {
      method: 'POST',
      cache: 'no-store',
      headers: { 'Accept': 'application/json' }
    }).then(function (response) {
      return response.text().then(function (text) {
        var payload = parseJsonSafe(text);
        if (!response.ok) throw makeHttpError(response.status, payload, describeStatus(response.status));
        if (!payload || payload.ok !== true) {
          throw makeHttpError(response.status, payload, '创建文件夹失败');
        }
        return payload;
      });
    }).then(function () {
      setMkdirPending(false);
      closeMkdirModal(true);
      toast('已创建文件夹「' + name + '」', 'success');
      loadDirectory();
    }).catch(function (error) {
      setMkdirPending(false);
      var message = error && error.message ? error.message : '创建文件夹失败';
      showMkdirError(message);
      toast(message, 'error');
    });
  }

  function onDocumentKeydown(event) {
    if (el.modalBackdrop.hidden) return;

    if (event.key === 'Escape' && !mkdirState.pending) {
      event.preventDefault();
      closeMkdirModal();
      return;
    }
    if (event.key !== 'Tab') return;

    var nodes = el.modalBackdrop.querySelectorAll('input, button, [href]');
    var focusable = [];
    for (var index = 0; index < nodes.length; index += 1) {
      var node = nodes[index];
      if (!node.disabled && node.getClientRects().length > 0) focusable.push(node);
    }
    if (!focusable.length) return;

    var first = focusable[0];
    var last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  /* ------------------------------------------------------------------------
   * 事件绑定 / 初始化
   * ---------------------------------------------------------------------- */

  function cacheElements() {
    el.brandSub = $('brandSub');
    el.modeBadge = $('modeBadge');
    el.refreshBtn = $('refreshBtn');
    el.writeActions = $('writeActions');
    el.uploadFileBtn = $('uploadFileBtn');
    el.uploadDirBtn = $('uploadDirBtn');
    el.newFolderBtn = $('newFolderBtn');
    el.fileInput = $('fileInput');
    el.dirInput = $('dirInput');
    el.readonlyBar = $('readonlyBar');
    el.catNav = $('catNav');
    el.navTotal = $('navTotal');
    el.breadcrumb = $('breadcrumb');
    el.upBtn = $('upBtn');
    el.sortBar = $('sortBar');
    el.groupedViewBtn = $('groupedViewBtn');
    el.flatViewBtn = $('flatViewBtn');
    el.contentCard = $('contentCard');
    el.statsLine = $('statsLine');
    el.dropOverlay = $('dropOverlay');
    el.dropHint = $('dropHint');
    el.transferPanel = $('transferPanel');
    el.transferTitle = $('transferTitle');
    el.transferCloseBtn = $('transferCloseBtn');
    el.progressBar = $('progressBar');
    el.progressFill = $('progressFill');
    el.progressText = $('progressText');
    el.progressDetail = $('progressDetail');
    el.transferBody = $('transferBody');
    el.modalBackdrop = $('modalBackdrop');
    el.mkForm = $('mkForm');
    el.mkName = $('mkName');
    el.mkError = $('mkError');
    el.mkCancelBtn = $('mkCancelBtn');
    el.mkSubmitBtn = $('mkSubmitBtn');
    el.mkCloseBtn = $('mkCloseBtn');
    el.toastRoot = $('toastRoot');
  }

  function bindEvents() {
    el.refreshBtn.addEventListener('click', function () {
      if (state.loading) return;
      loadDirectory();
    });

    el.upBtn.addEventListener('click', onUpClick);
    el.uploadFileBtn.addEventListener('click', triggerFilePicker);
    el.uploadDirBtn.addEventListener('click', triggerDirPicker);
    el.newFolderBtn.addEventListener('click', openMkdirModal);

    el.fileInput.addEventListener('change', onFileInputChange);
    el.dirInput.addEventListener('change', onDirInputChange);

    el.groupedViewBtn.addEventListener('click', function () { setView('grouped'); });
    el.flatViewBtn.addEventListener('click', function () { setView('flat'); });

    var sortButtons = el.sortBar.querySelectorAll('[data-sort]');
    for (var index = 0; index < sortButtons.length; index += 1) {
      (function (button) {
        button.addEventListener('click', function () {
          setSort(button.getAttribute('data-sort'));
        });
      })(sortButtons[index]);
    }

    el.contentCard.addEventListener('click', onContentClick);

    el.catNav.addEventListener('click', function (event) {
      var target = event.target;
      var button = target && target.closest ? target.closest('.cat-btn') : null;
      if (!button) return;
      jumpToCategory(button.getAttribute('data-category'));
    });

    el.mkForm.addEventListener('submit', function (event) {
      event.preventDefault();
      submitMkdir();
    });
    el.mkCancelBtn.addEventListener('click', function () { closeMkdirModal(); });
    el.mkCloseBtn.addEventListener('click', function () { closeMkdirModal(); });
    el.modalBackdrop.addEventListener('mousedown', function (event) {
      if (event.target === el.modalBackdrop) closeMkdirModal();
    });
    document.addEventListener('keydown', onDocumentKeydown);

    el.transferCloseBtn.addEventListener('click', onTransferClose);

    document.addEventListener('dragenter', onDragEnter);
    document.addEventListener('dragover', onDragOver);
    document.addEventListener('dragleave', onDragLeave);
    document.addEventListener('drop', onDrop);
    document.addEventListener('dragend', hideDropOverlay);
    window.addEventListener('blur', hideDropOverlay);

    window.addEventListener('popstate', onPopState);
  }

  function init() {
    cacheElements();
    if (!el.contentCard || !el.breadcrumb) return;
    state.dir = dirFromPathname(window.location.pathname);
    bindEvents();
    loadDirectory();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
