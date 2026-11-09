import { invoke } from '@tauri-apps/api/core';
import './style.css';

const state = {
  config: null,
  running: false,
  logs: [],
  addresses: [],
  timer: null,
};

const app = document.querySelector('#app');

app.innerHTML = `
  <div class="shell">
    <header class="topbar">
      <div>
        <div class="title">轻型文件服务器</div>
        <div class="subtitle">Tauri · FTP + HTTP · 匿名访问</div>
      </div>
      <div id="statusBadge" class="status off">● 已停止</div>
    </header>

    <main class="content">
      <section class="card hero">
        <div>
          <div class="section-title">共享目录</div>
          <div class="path-row">
            <input id="rootDir" class="path-input" />
            <button id="browseBtn" class="secondary">浏览</button>
          </div>
          <div class="hint">FTP 与 HTTP 都共享这个目录。请确认目录中没有不应公开的数据。</div>
        </div>
        <div class="actions">
          <button id="startBtn" class="primary">启动服务器</button>
          <button id="stopBtn" class="danger" disabled>停止服务器</button>
        </div>
      </section>

      <section class="grid">
        <div class="card">
          <div class="section-title">服务设置</div>
          <div class="form-grid">
            <label>启动服务
              <select id="serviceMode">
                <option value="both">FTP + HTTP</option>
                <option value="ftp">仅 FTP</option>
                <option value="http">仅 HTTP</option>
              </select>
            </label>
            <div></div>
            <label>FTP 端口<input id="ftpPort" type="number" min="1" max="65535" /></label>
            <label>HTTP 端口<input id="httpPort" type="number" min="1" max="65535" /></label>
            <label>被动端口起始<input id="pasvStart" type="number" min="1024" max="65534" /></label>
            <label>被动端口结束<input id="pasvEnd" type="number" min="1025" max="65535" /></label>
            <label class="wide">对外地址（可留空自动检测）<input id="publicHost" placeholder="例如 192.168.1.100" /></label>
            <label class="check wide"><input id="anonymous" type="checkbox" />允许匿名访问（用户名 anonymous / 密码任意）</label>
            <label class="check wide"><input id="anonymousWrite" type="checkbox" />允许匿名上传、创建和删除文件（默认关闭）</label>
          </div>
          <div class="form-actions"><button id="saveBtn" class="secondary">保存设置</button><button id="resetBtn" class="ghost">恢复默认</button></div>
        </div>

        <div class="card">
          <div class="section-title">访问地址</div>
          <div class="endpoint"><span>HTTP</span><code id="httpUrl">—</code><button data-copy="httpUrl">复制</button></div>
          <div class="endpoint"><span>FTP</span><code id="ftpUrl">—</code><button data-copy="ftpUrl">复制</button></div>
          <div class="section-title small-title">本机网络地址</div>
          <div id="addresses" class="addresses">正在获取…</div>
          <div class="hint">局域网其他设备访问时，请使用下面显示的本机 IPv4 地址。</div>
        </div>
      </section>

      <section class="card log-card">
        <div class="section-title">运行日志</div>
        <pre id="logs" class="logs"></pre>
      </section>
    </main>
    <footer>匿名 FTP 是明文协议，不适合公网传输敏感数据。HTTP 服务同样默认不启用 HTTPS。</footer>
  </div>
`;

function $(id) { return document.getElementById(id); }

function collectConfig() {
  return {
    service_mode: $('serviceMode').value,
    root_dir: $('rootDir').value.trim(),
    ftp_port: Number($('ftpPort').value),
    http_port: Number($('httpPort').value),
    passive_start: Number($('pasvStart').value),
    passive_end: Number($('pasvEnd').value),
    public_host: $('publicHost').value.trim(),
    anonymous_enabled: $('anonymous').checked,
    anonymous_write_enabled: $('anonymousWrite').checked,
  };
}

function applyConfig(c) {
  state.config = c;
  $('serviceMode').value = c.service_mode || 'both';
  $('rootDir').value = c.root_dir || '';
  $('ftpPort').value = c.ftp_port;
  $('httpPort').value = c.http_port;
  $('pasvStart').value = c.passive_start;
  $('pasvEnd').value = c.passive_end;
  $('publicHost').value = c.public_host || '';
  $('anonymous').checked = c.anonymous_enabled;
  $('anonymousWrite').checked = c.anonymous_write_enabled;
  renderEndpoints();
}

function hostForUrls() {
  return state.config?.public_host || state.addresses[0] || '127.0.0.1';
}

function renderEndpoints() {
  if (!state.config) return;
  const host = hostForUrls();
  const mode = state.config.service_mode || 'both';
  const httpOn = mode === 'http' || mode === 'both';
  const ftpOn = mode === 'ftp' || mode === 'both';
  $('httpUrl').textContent = httpOn ? `http://${host}:${state.config.http_port}/` : '未启动';
  $('ftpUrl').textContent = ftpOn ? `ftp://anonymous@${host}:${state.config.ftp_port}/` : '未启动';
}

function renderStatus(running) {
  state.running = running;
  $('statusBadge').className = `status ${running ? 'on' : 'off'}`;
  $('statusBadge').textContent = running ? '● 运行中' : '● 已停止';
  $('startBtn').disabled = running;
  $('stopBtn').disabled = !running;
  ['serviceMode','rootDir','ftpPort','httpPort','pasvStart','pasvEnd','publicHost','anonymous','anonymousWrite','browseBtn','saveBtn','resetBtn']
    .forEach(id => $(id).disabled = running);
}

function renderLogs() {
  $('logs').textContent = state.logs.join('\n');
  $('logs').scrollTop = $('logs').scrollHeight;
}

async function refresh() {
  try {
    const [status, logs, addresses] = await Promise.all([
      invoke('get_status'), invoke('get_logs'), invoke('get_local_addresses')
    ]);
    renderStatus(status.running);
    state.logs = logs;
    state.addresses = addresses;
    $('addresses').innerHTML = addresses.length
      ? addresses.map(x => `<div class="address-line"><span>${x}</span><span class="address-copy" data-value="${x}">复制</span></div>`).join('')
      : '<div class="muted">没有检测到 IPv4 地址</div>';
    renderEndpoints();
    renderLogs();
  } catch (e) {
    console.error(e);
  }
}

async function start() {
  try {
    const config = collectConfig();
    await invoke('save_config', { config });
    await invoke('start_server', { config });
    applyConfig(config);
    await refresh();
  } catch (e) {
    alert(`启动失败：${e}`);
  }
}

async function stop() {
  try {
    await invoke('stop_server');
    await refresh();
  } catch (e) {
    alert(`停止失败：${e}`);
  }
}

$('browseBtn').onclick = async () => {
  try {
    const picked = await invoke('pick_directory');
    if (picked) $('rootDir').value = picked;
  } catch (e) { alert(`选择目录失败：${e}`); }
};
$('startBtn').onclick = start;
$('stopBtn').onclick = stop;
$('saveBtn').onclick = async () => {
  try {
    const config = collectConfig();
    await invoke('save_config', { config });
    applyConfig(config);
    await refresh();
    alert('设置已保存');
  } catch (e) { alert(`保存失败：${e}`); }
};
$('resetBtn').onclick = async () => {
  try { applyConfig(await invoke('reset_config')); } catch (e) { alert(`恢复默认失败：${e}`); }
};
document.addEventListener('click', async (e) => {
  const copyId = e.target.dataset.copy;
  const copyValue = e.target.dataset.value;
  if (copyId) { await navigator.clipboard.writeText($(copyId).textContent); }
  if (copyValue) { await navigator.clipboard.writeText(copyValue); }
});

(async () => {
  try { applyConfig(await invoke('get_config')); } catch (e) { alert(`读取配置失败：${e}`); }
  await refresh();
  state.timer = setInterval(refresh, 1200);
})();
