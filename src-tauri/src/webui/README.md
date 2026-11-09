# HTTP 文件浏览页（浏览器端 UI）

本目录的 `index.html`、`app.css`、`app.js` 会被 Rust 后端用 `include_str!` 内嵌进
HTTP 服务，作为浏览器访问 `http://<主机>:<端口>/` 时的文件浏览页面。页面为纯静态
资源，运行时通过下面的 JSON API 获取数据，不依赖任何 CDN 或前端框架（局域网可离线）。

三份文件与后端接口的约定如下，改动任一侧时需同步更新本文件。

## 静态资源路由

| 路由 | 内容 | Content-Type |
| --- | --- | --- |
| `GET /_assets/app.css` | `app.css` | `text/css; charset=utf-8` |
| `GET /_assets/app.js` | `app.js` | `text/javascript; charset=utf-8` |

`index.html` 通过绝对路径 `/_assets/app.css`、`/_assets/app.js` 引用资源，因此页面
在任意子目录下都能正常工作。

## `GET /_api/list?dir=<虚拟目录绝对路径>`

列出目录内容。`dir` 省略时默认 `/`，值需 URL 编码（如 `/?dir=%2F%E5%85%B1%E4%BA%AB`）。

成功（200）：

```json
{
  "ok": true,
  "dir": "/sub",
  "parent": "/",
  "writable": true,
  "entries": [
    {
      "name": "报告.pdf",
      "path": "/sub/报告.pdf",
      "is_dir": false,
      "size": 12345,
      "modified": 1762650000,
      "ext": "pdf"
    },
    {
      "name": "图片",
      "path": "/sub/图片",
      "is_dir": true,
      "size": 0,
      "modified": 1762650000,
      "ext": ""
    }
  ]
}
```

- `path` 是可直接拼到 `location.origin` 后面的下载链接（每段已做 URL 编码）。
- `modified` 为 Unix 秒（缺失时为 `null`）；`size` 仅对文件有意义。
- `ext` 为小写、不含点的扩展名；无扩展名或目录为空字符串。
- `writable` 表示服务器是否开启匿名写入（决定页面是否显示上传/新建文件夹控件）。
- 失败（403/404/500）返回 `{"ok": false, "error": "中文错误信息"}`。

## `POST /_api/upload?dir=<虚拟目录绝对路径>&path=<相对路径>`

上传**单个**文件；文件夹上传由前端对其中每个文件各发一次请求完成。

- 请求体为文件的原始字节流（建议 `Content-Type: application/octet-stream`），后端必须流式
  写入磁盘（不要整文件读入内存），自动创建缺少的中间目录，禁止越出共享根目录。
- `path` 是相对 `dir` 的路径，可包含子目录（例如 `相册/2024/a.jpg`），前端需
  `encodeURIComponent` 后放进查询参数。
- 成功（200）：`{"ok": true, "path": "/当前目录/相对路径"}`。
- 失败（400/403/404/409/500）：`{"ok": false, "error": "中文原因"}`；写入功能关闭时 403
  `{"ok": false, "error": "上传功能未启用"}`。

## `POST /_api/mkdir?dir=<虚拟父目录>&name=<新文件夹名>`

创建文件夹。成功（200）：`{"ok": true, "path": "/父/新文件夹"}`。
已存在返回 409；名称非法（含 `/`、`\`、`: `、`..` 等）返回 400；写入关闭返回 403。
失败统一返回 `{"ok": false, "error": "中文错误信息"}`。

## 页面行为（`app.js`）

- 从 `location.pathname` 解析当前目录（以 `/` 结尾），渲染面包屑、条目表格。
- 排序：名称 / 大小 / 修改时间 / 类型（后缀），每列可切换升序、降序；文件夹始终排在文件前。
- 分类展示：按扩展名分为 文件夹、图片、视频、音频、文档、压缩包、代码、程序、字体、其他，
  每组显示数量；支持切换「分类展示 / 平铺列表」。
- 上传：上传文件（多选）、上传文件夹（`webkitdirectory`）、拖拽文件或文件夹到页面；
  文件夹拖拽通过 `DataTransferItem.webkitGetAsEntry()` 递归展开并保留相对路径。
  每个文件单独 `XMLHttpRequest` POST 到 `_api/upload`，用 `upload.onprogress` 累计总进度，
  结束后刷新列表并提示成功/失败数量。
- 新建文件夹：输入名称后调用 `_api/mkdir`，成功后刷新列表。
- `writable` 为 `false` 时隐藏上传/新建控件并显示「只读模式」提示。
- 所有错误用页面内提示（toast/提示条），不要使用会阻塞的 `alert`。
- 中文界面，支持窄屏（手机浏览器）。

## 约束

- 不得引用任何外部 URL（字体、图标、脚本、样式均需内联或本地）。
- `app.css` 与 `app.js` 会内嵌进二进制，注意体积；不要使用构建工具或 import 语法。
- 安全：所有动态文本插入 DOM 时必须转义或使用 `textContent`，禁止把文件名直接拼进
  `innerHTML` 造成 XSS。
