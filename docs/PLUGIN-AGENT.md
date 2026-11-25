# YaJarvis Plugin Spec for AI Coding Agents (`PLUGIN-AGENT.md`)

> **Как использовать этот файл:**
> - **В ChatGPT / Claude / Gemini:** вставьте промпт:
>   `Прочитай https://raw.githubusercontent.com/newfpv/YaJarvis/main/docs/PLUGIN-AGENT.md и напиши плагин YaJarvis для <описание задачи>.`
> - **В Cursor / Claude Code / Codex / Antigravity:** скопируйте этот файл в корень репозитория вашего плагина как `AGENTS.md` или `CLAUDE.md`.

Ниже дана точная инженерная спецификация рантайма YaJarvis (`0.10.x`, `api: 1`), сверенная с исходным кодом ядра `jarvisd`. Следуй ей строго, без догадок и устаревших форматов.

---

## 1. Выбор типа плагина: Lua или Rust

| Задача плагина | Что выбрать | Почему |
| :--- | :--- | :--- |
| HTTP/HTTPS API, парсинг JSON, быстрые голосовые команды, инструменты LLM, таймеры, кеш/состояние, свой LLM / STT / TTS | **`kind: "lua"`** (по умолчанию!) | Один файл `main.lua`, строгая песочница, работает на всех архитектурах (`arm64` и `armv7`), один архив `jarvis-plugin.zip`, сборка не нужна |
| Системные команды Linux, шелл-скрипты, `iptables`, сокеты, бинарные протоколы, локальная ФС, сложная криптография или тяжёлые вычисления | **`kind: "rust"`** | Нативный бинарник с правами `jarvisd` через `jarvis-plugin-sdk`, нужны сборки под `arm64` и `armv7` |

**Правило:** всегда начинай с **`kind: "lua"`**, если задача решается через HTTP-запросы, таймеры и JSON. Переходи на **`kind: "rust"`** только если без доступа к ОС/сокетам/библиотекам Rust не обойтись.

---

## 2. Жёсткие правила (NEVER / MUST)

Эти 15 пунктов закрывают почти все реальные ошибки, из-за которых плагины не устанавливаются, не видятся в каталоге или молча не работают на колонке:

1. **NEVER** используй устаревшие `kind: "js"` или `kind: "native"`. Допустимы только `"lua"` и `"rust"`, поле `"api"` всегда равно `1`.
2. **NEVER** занимай зарезервированные ID официальных плагинов: `chatgpt`, `openai`, `gemini`, `grok`, `elevenlabs`, `homeassistant`, `yandexhome`, `smartlife`, `xiaomi`, `vpn`, `youtube`, `training`.
3. **NEVER** называй инструменты именами из `CORE_TOOLS`: `smart_home`, `play_music`, `music_control`, `alice_command`, `recall_conversation`, `edit_memory`, `home_assistant`, `xiaomi_home`, `smart_life`.
4. **NEVER** используй вложенный ключ `"function": {...}` в Rust `Plugin::tools()`! Ядро проверяет на верхнем уровне `tool["type"] == "function"`, `tool["name"]`, `tool["parameters"].is_object()` и **молча отбрасывает** инструменты с вложенным `"function"`.
5. **NEVER** рассчитывай на `PATH` или переменные среды в Rust-плагинах: ядро запускает процесс с `.env_clear()`. Всегда вызывай `/bin/sh` по полному пути и передавай `.env("PATH", "/opt/sbin:/opt/bin:/usr/sbin:/usr/bin:/sbin:/bin")`.
6. **NEVER** выполняй скачивание, установку или долгие команды синхронно внутри Rust `init` (таймаут **5 с**), `command` (таймаут **2 с**) или `tool` (таймаут **15 с**). При превышении таймаута ядро **убивает процесс плагина**. Выноси долгую работу в `tokio::spawn` и используй `tokio::process::Command` вместо блокирующего `std::process::Command`.
7. **NEVER** пиши в `stdout` (`println!`) в Rust-плагинах. `stdout` полностью занят бинарным RPC-протоколом; весь лог пиши строго в `stderr` (`eprintln!`).
8. **NEVER** оставляй `"arch": "arm64"` внутри `jarvis-plugin-armv7.zip`. Ядро сверяет `arch` в `plugin.json` с ELF-заголовком бинарника и отклонит установку.
9. **NEVER** используй в Lua запрещённые модули: `os`, `io`, `package`, `require`, `debug`, `coroutine`, `dofile`, `loadfile`, `load`, `collectgarbage`. Весь Lua-код обязан лежать в одном файле `main.lua` (<= 512 КБ). Вместо `os.time()` используй `math.floor(jarvis.now() / 1000)`.
10. **NEVER** используй `string.lower()` для кириллицы в Lua (он ломает UTF-8). Используй `jarvis.norm(s)` или `jarvis.normalize(s)`.
11. **MUST** синхронизировать `permissions`, `provides` и экспорты в Lua:
    - есть `tools` -> нужен `"tools"` в `permissions`;
    - есть `command` -> нужен `"commands"` в `permissions`;
    - есть `jarvis.fetch` -> нужен `"network"` в `permissions` и домены в `network_allow` (важно: `*.example.com` НЕ включает корневой `example.com`, при необходимости укажи оба);
    - есть `jarvis.store` -> нужен `"storage"`, `jarvis.timers` -> `"timers"`, `jarvis.say` -> `"say"`;
    - строгий паритет `provides`: `"llm"` <-> `chat`, `"stt"` <-> `transcribe`, `"tts"` <-> `speak` (если указал в `provides`, обязан экспортировать функцию, и наоборот).
12. **NEVER** перехватывай в `command(text)` слишком широкие подстроки (например `text:find("погода")` перехватит фразу `"переведи слово погода"`). Если фраза не ваша, возвращай `nil` (Lua) или `Ok(Value::Null)` (Rust).
13. **MUST** возвращать короткий естественный текст для озвучки голосом (без Markdown-разметки, звёздочек, ссылок, таблиц и эмодзи).
14. **MUST** выбирать `icon` строго из списка 20 встроенных иконок панели (любое другое имя откатывается к `puzzle`).
15. **MUST** публиковать плагин в **обычном публичном не-архивном репозитории (НЕ форке!)** с темой `yajarvis-plugin` + темой категории (`yajarvis-...`), обычным (не Draft / не Pre-release) GitHub Release и точным именем ассета: `jarvis-plugin.zip` (Lua) или `jarvis-plugin-arm64.zip` / `jarvis-plugin-armv7.zip` (Rust).

---

## 3. Спецификация `plugin.json`

`plugin.json` лежит в корне ZIP-архива (или в одной корневой подпапке). Максимальный размер `plugin.json`: 256 КБ; ZIP: 24 МБ; распакованный пакет: 48 МБ. Симлинки, `..` и абсолютные пути в ZIP запрещены.

### Обязательные и опциональные поля

```json
{
  "id": "my-plugin",
  "name": "Мой плагин",
  "version": "1.0.0",
  "description": "Короткое описание для карточки в панели.",
  "author": "your-github-login",
  "homepage": "https://github.com/your-github-login/yajarvis-my-plugin",
  "kind": "lua",
  "main": "main.lua",
  "api": 1,
  "category": "info",
  "icon": "bolt",
  "provides": ["tools", "commands"],
  "permissions": ["tools", "commands", "network", "storage"],
  "network_allow": ["api.example.com"],
  "min_core": "0.10.5",
  "reboot_on_update": false,
  "settings": [
    {
      "key": "api_key",
      "label": "API-ключ",
      "type": "password",
      "hint": "Подсказка по знаку ?",
      "default": "",
      "required": true
    }
  ]
}
```

### Точные ограничения полей

- `id`: regex `^[a-z0-9][a-z0-9-]{1,31}$` (2..32 символа, латиница в нижнем регистре, цифры, дефис). НЕ из списка официальных ID.
- `name`: непустая строка.
- `version`: SemVer (`"1.0.0"`), увеличивается при каждом релизе.
- `kind`: `"lua"` или `"rust"`.
- `main`:
  - для Lua: `"main.lua"` (или имя файла без `..`, `/`, `\`);
  - для Rust: строго имя бинарника в корне ZIP (`[a-zA-Z0-9._-]`, без `/`, не `data`, `data.json`, `plugin.json`, `.`). Ядро само выставит права `0700` на `main`, а на остальные файлы из ZIP поставит `0644`.
- `arch`: обязательно для `kind: "rust"`, строго `"arm64"` или `"armv7"`.
- `api`: всегда `1`.
- `category`: строго одно из 9 значений (и соответствующая тема на GitHub `yajarvis-<category>`):
  - `assistant`, `home`, `media`, `info`, `productivity`, `dev`, `fun`, `system`, `other`.
- `icon`: строго одно из 20 встроенных имён:
  - `puzzle`, `shield`, `cloud`, `wifi`, `home`, `bolt`, `music`, `radio`, `mic`, `speaker`, `voice`, `moon`, `heart`, `search`, `tune`, `link`, `list`, `refresh`, `play`, `done`.
- `provides`: массив из `tools`, `commands`, `llm`, `stt`, `tts`. (НЕ указывай `vpn`, `homeassistant`, `xiaomi`, `smartlife`, `yandexhome` - они зарезервированы для официальных пакетов.)
- `permissions` (для Lua): массив из `tools`, `commands`, `say`, `network`, `storage`, `timers`.
- `network_allow` (для Lua с `network`): массив точных хостов (`api.example.com`), origin (`https://api.example.com:8443`) или поддоменов (`*.example.com`).
- `settings[].type`: `text`, `password`, `bool`, `number`, `select` (требует `"options": ["..."]`), `textarea`.

---

## 4. Точный контракт Lua (`kind: "lua"`)

### Песочница и лимиты
- Lua 5.4, библиотеки: `table`, `string`, `math`, `utf8`, базовые функции.
- Память VM: **12 МБ**, размер `main.lua`: **512 КБ**, очередь вызовов: **16**.
- Лимит CPU на один вызов: **2 с** инструкций Lua.
- Лимит wall-clock на вызов (включая HTTP): **20 с** (`init`, `configure`, `command`, `tools`, `chat`, таймеры), **30 с** (`transcribe`, `speak`).

### Доступный API `jarvis.*`
- Всегда доступны:
  - `jarvis.settings` - таблица текущих настроек с подставленными `default`.
  - `jarvis.info` - `{ city, timezone, room, language, model, assistant }`.
  - `jarvis.log(text)` - запись в журнал колонки (до 600 символов).
  - `jarvis.status(text)` - короткий статус в карточке панели (до 200 символов).
  - `jarvis.json.encode(val)` / `jarvis.json.decode(str)` - JSON (при ошибке бросают Lua-ошибку -> оборачивай `decode` в `pcall`!).
  - `jarvis.now()` - Unix timestamp в **миллисекундах** (integer).
  - `jarvis.normalize(text)` - нижний регистр, `ё -> е`, без пунктуации, без ведущего `джарвис` / `jarvis`.
  - `jarvis.norm(text)` - нижний регистр, `ё -> е`, без пунктуации (без удаления `джарвис`).
  - `jarvis.urlencode(text)` - percent-encoding для query-строк.
  - `jarvis.random(min, max)` - криптографически стойкое целое в `[min, max]`.
- По правам в `permissions`:
  - `"say"`: `jarvis.say(text)` (до 600 символов).
  - `"storage"`: `jarvis.store.get(key)` / `jarvis.store.set(key, val)` (`nil` удаляет; ключ 1..256 байт; всего <= 256 КБ; файл `plugins-data/<id>.json` переживает обновление).
  - `"timers"`: `jarvis.timers.after(ms, fn)`, `jarvis.timers.every(ms, fn)` (от 100 мс до 7 суток, до 64 таймеров), `jarvis.timers.cancel(id)`.
  - `"network"` + `network_allow`: `jarvis.fetch(url, opts)`:
    - `opts`: `{ method = "GET"|"POST"|..., headers = { ["K"] = "V" }, timeout_ms = 1000..25000, body = "..." (<=1 МБ) | body_base64 = "..." (<=3 МБ) | multipart = { { name="...", value="..." } | { name="...", filename="...", content_type="...", data_base64="..." } } (<=4 МБ), response_encoding = "utf8"|"base64" }`.
    - Возвращает `{ status = number, ok = boolean, body = string }` (или `body_base64` при `response_encoding = "base64"`).
    - **Важно:** HTTP 4xx/5xx НЕ бросают исключение (`res.ok == false`), а ошибки сети/VPN/таймаута/редиректа/allowlist БРОСАЮТ Lua-ошибку -> всегда оборачивай `jarvis.fetch` и `jarvis.json.decode` в `pcall`!
    - Редиректы: до 5 для `GET`/`HEAD`, запрещён переход `https -> http`, запрещён редирект с пользовательскими `headers` на другой origin, запрещены URL с `user:pass@`.

### Возвращаемая таблица `main.lua`

```lua
local function safe_get_json(url)
  local ok_req, res = pcall(jarvis.fetch, url, { timeout_ms = 6000 })
  if not ok_req then
    return nil, "Ошибка сети: " .. tostring(res)
  end
  if not res.ok then
    return nil, "Сервис вернул HTTP " .. tostring(res.status)
  end
  local ok_json, data = pcall(jarvis.json.decode, res.body)
  if not ok_json or type(data) ~= "table" then
    return nil, "Некорректный JSON в ответе"
  end
  return data, nil
end

return {
  init = function()
    jarvis.status("Готов")
  end,

  configure = function(settings)
    -- jarvis.settings уже обновлён
    jarvis.status("Настройки применены")
  end,

  command = function(text)
    -- text уже нормализован: нижний регистр, ё -> е, без пунктуации, без «джарвис»
    if text == "проверь мой сервис" or text == "статус моего сервиса" then
      return "Сервис работает штатно."
    end
    return nil
  end,

  tools = {
    {
      name = "my_service_query",
      description = "Получает данные из моего сервиса по запросу пользователя.",
      parameters = {
        type = "object",
        properties = {
          query = { type = "string", description = "Что нужно найти" },
        },
        required = { "query" },
      },
      handler = function(args)
        local q = type(args) == "table" and tostring(args.query or "") or ""
        if q == "" then return "Укажите запрос." end
        local data, err = safe_get_json("https://api.example.com/search?q=" .. jarvis.urlencode(q))
        if err then return err end
        return tostring(data.summary or "Ничего не найдено.")
      end,
    },
  },
}
```

### Контракты провайдеров (если указаны в `provides`)
- `"llm"` -> `chat(req)`: `req = { messages, model, tools }`; вернуть строку или `{ text = "", tool_calls = { { id = "call_1", name = "...", arguments = "{...}" } } }`.
- `"stt"` -> `transcribe(req)`: `req = { audio_b64, format = "wav", rate = 16000, model, language }`; вернуть `{ text = "..." }` или строку.
- `"tts"` -> `speak(req)` (+ опционально `voices()`): `req = { text, voice, model, language }`; вернуть `{ audio = "<base64 PCM16 mono LE>", rate = 16000|24000|48000 }`. `voices()` возвращает `{ { id = "...", name = "...", note = "..." }, ... }`.

---

## 5. Точный контракт Rust (`kind: "rust"`)

### `Cargo.toml`

```toml
[package]
name = "my-plugin"
version = "1.0.0"
edition = "2024"

[dependencies]
jarvis-plugin-sdk = { git = "https://github.com/newfpv/YaJarvis", package = "jarvis-plugin-sdk", features = ["process"] }
serde_json = "1"
tokio = { version = "1", features = ["rt", "macros", "process", "sync", "time"] }

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
```

### Методы RPC, форматы и жёсткие таймауты

| Метод | Вход | Выход | Таймаут |
| :--- | :--- | :--- | :--- |
| `init` | `Init { settings, info }` в `process::serve` | `Ok(Box::new(plugin))` / `Err(msg)` | **5 с** (при провале установка откатывается!) |
| `configure` | `Plugin::configure(&self, settings)` | обновить внутренний `RwLock`/`Mutex` | **5 с** |
| `"command"` | `params`: JSON-строка (нормализованная фраза) | `Ok(json!("Ответ"))` или `Ok(Value::Null)` | **2 с** |
| `"tool"` | `params`: `{"name": "...", "arguments": {...}}` | `Ok(json!("Ответ"))` или `Err(msg)` | **15 с** |
| `"llm.chat"` | `{ messages, model, tools }` | строка или `{ text, tool_calls }` | **30 с** |
| `"stt.transcribe"` | `{ audio_b64, format, rate, model, language }` | `{ text }` или строка | **45 с** |
| `"tts.speak"` | `{ text, voice, model, language }` | `{ audio, rate }` | **45 с** |
| `"shutdown"` | вызывает `Plugin::stop(&self)` | выход из процесса | **1 с** |

### Среда исполнения процесса
- Рабочая директория (`std::env::current_dir()`): `/data/opt/jarvisd/plugins/<id>/data`. Всё в `./` и `../data.json` сохраняется при обновлениях.
- Остальные файлы из ZIP лежат в родительской директории (`current_dir().parent()`) с правами `0644`.
- Переменные окружения пустые (`env_clear()`). Задавай `PATH` явно при запуске любых команд.
- Ошибки `Err(String)` ядро обрезает до 300 символов и передаёт в LLM/панель.

### Эталонный шаблон `src/main.rs`

```rust
use jarvis_plugin_sdk::{Map, Plugin, Response, Value, json, process};
use std::sync::{Arc, RwLock};
use tokio::process::Command;

const DEFAULT_PATH: &str = "/opt/sbin:/opt/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone)]
struct State {
    enabled: bool,
}

struct MyPlugin {
    state: Arc<RwLock<State>>,
}

impl MyPlugin {
    fn from_settings(settings: &Map<String, Value>) -> State {
        State {
            enabled: settings.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        }
    }
}

async fn run_sh(cmd: &str) -> Result<String, String> {
    let out = Command::new("/bin/sh")
        .args(["-c", cmd])
        .env("PATH", DEFAULT_PATH)
        .output()
        .await
        .map_err(|e| format!("не удалось запустить /bin/sh: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        Err(if msg.is_empty() {
            format!("код выхода {}", out.status)
        } else {
            msg
        })
    }
}

impl Plugin for MyPlugin {
    // ПЛОСКИЙ формат! Без вложенного ключа "function"!
    fn tools(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "name": "my_plugin_action",
            "description": "Выполняет действие моего плагина и возвращает короткий результат.",
            "parameters": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["status", "restart"],
                        "description": "Режим работы"
                    }
                },
                "required": ["mode"]
            }
        })]
    }

    fn configure(&self, settings: Map<String, Value>) {
        if let Ok(mut st) = self.state.write() {
            *st = Self::from_settings(&settings);
        }
    }

    fn request(&self, method: &str, params: Value) -> Response<'_> {
        // Копируем method до Box::pin, чтобы избежать ошибки lifetime на &str
        let method = method.to_owned();
        let state = self.state.clone();

        Box::pin(async move {
            match method.as_str() {
                "command" => {
                    let text = params.as_str().unwrap_or("");
                    match text {
                        "статус плагина" => {
                            let on = state.read().map(|s| s.enabled).unwrap_or(false);
                            Ok(json!(if on { "Плагин включён." } else { "Плагин выключен." }))
                        }
                        "перезапусти плагин" => {
                            // Таймаут command = 2 с -> долгую работу уносим в фон!
                            tokio::spawn(async move {
                                if let Err(e) = run_sh("echo restarted").await {
                                    eprintln!("my-plugin: bg error: {e}");
                                }
                            });
                            Ok(json!("Запустил перезапуск в фоне."))
                        }
                        _ => Ok(Value::Null),
                    }
                }
                "tool" => {
                    let name = params["name"].as_str().unwrap_or("");
                    let args = &params["arguments"];
                    match name {
                        "my_plugin_action" => {
                            let mode = args["mode"].as_str().unwrap_or("status");
                            match mode {
                                "status" => Ok(json!("Работает штатно.")),
                                "restart" => {
                                    tokio::spawn(async move {
                                        let _ = run_sh("echo restarted").await;
                                    });
                                    Ok(json!("Перезапуск запущен в фоне."))
                                }
                                _ => Err(format!("Неизвестный режим: {mode}")),
                            }
                        }
                        _ => Err(format!("Неизвестный инструмент: {name}")),
                    }
                }
                _ => Ok(Value::Null),
            }
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    process::serve(|init| {
        Ok(Box::new(MyPlugin {
            state: Arc::new(RwLock::new(MyPlugin::from_settings(&init.settings))),
        }))
    })
    .await
}
```

---

## 6. Готовые шаблоны GitHub Actions для автоматических релизов

Если создаёшь репозиторий плагина с нуля, сразу добавь подходящий workflow в `.github/workflows/release.yml`, чтобы при пуше тега `v1.0.0` GitHub сам собрал и опубликовал правильный ZIP!

### Вариант A: Lua-плагин (`.github/workflows/release.yml`)

```yaml
name: Release plugin
on:
  push:
    tags: ['v*']
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Validate manifest and pack jarvis-plugin.zip
        run: |
          jq -e '.kind == "lua" and .api == 1 and (.id | test("^[a-z0-9][a-z0-9-]{1,31}$"))' plugin.json >/dev/null
          test -f "$(jq -r .main plugin.json)"
          rm -f jarvis-plugin.zip
          zip -j jarvis-plugin.zip plugin.json "$(jq -r .main plugin.json)"
      - name: Publish GitHub Release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh release create "${GITHUB_REF_NAME}" jarvis-plugin.zip --title "${GITHUB_REF_NAME}" --generate-notes
```

### Вариант B: Rust-плагин (`.github/workflows/release.yml`)

```yaml
name: Release Rust plugin
on:
  push:
    tags: ['v*']
permissions:
  contents: write
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-unknown-linux-musl,armv7-unknown-linux-musleabihf
      - uses: mlugg/setup-zig@v2
        with:
          version: 0.15.2
      - run: cargo install cargo-zigbuild --locked
      - name: Build arm64 & armv7 packages
        run: |
          BIN="$(jq -r .main plugin.json)"
          pack() {
            target="$1"; arch="$2"
            cargo zigbuild --release --target "$target"
            rm -rf "dist/$arch" "jarvis-plugin-$arch.zip"
            mkdir -p "dist/$arch"
            cp "target/$target/release/$BIN" "dist/$arch/$BIN"
            jq --arg a "$arch" '.arch = $a' plugin.json > "dist/$arch/plugin.json"
            jq -e --arg a "$arch" '.kind == "rust" and .api == 1 and .arch == $a' "dist/$arch/plugin.json" >/dev/null
            (cd "dist/$arch" && zip -r "../../jarvis-plugin-$arch.zip" .)
          }
          pack aarch64-unknown-linux-musl arm64
          pack armv7-unknown-linux-musleabihf armv7
      - name: Publish GitHub Release
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          gh release create "${GITHUB_REF_NAME}" jarvis-plugin-arm64.zip jarvis-plugin-armv7.zip \
            --title "${GITHUB_REF_NAME}" --generate-notes
```

---

## 7. Обязательная самопроверка агента перед завершением

Перед тем как отдавать код пользователю, проверь каждый пункт:

1. **Манифест `plugin.json`**:
   - валидный JSON, `"api": 1`, `"kind"` это `"lua"` или `"rust"`;
   - `"id"` соответствует `^[a-z0-9][a-z0-9-]{1,31}$` и не входит в список официальных ID;
   - `"category"` из 9 допустимых значений;
   - `"icon"` из 20 встроенных иконок панели;
   - `"main"` реально совпадает с именем файла скрипта или бинарника.
2. **Для Lua**:
   - нет `os`, `io`, `require`, `package`, `debug`, `coroutine`, `load`, `dofile`, `loadfile`, `string.lower` по кириллице;
   - все используемые `jarvis.fetch` / `jarvis.store` / `jarvis.timers` / `jarvis.say` / `tools` / `command` имеют соответствующие права в `permissions`, а все хосты URL есть в `network_allow`;
   - `jarvis.fetch` и `jarvis.json.decode` обёрнуты в `pcall`;
   - `provides` строго совпадает с экспортами `chat` / `transcribe` / `speak`;
   - `command(text)` возвращает `nil` на чужие фразы и не перехватывает случайные подстроки.
3. **Для Rust**:
   - `Plugin::tools()` возвращает **плоский** JSON без вложенного `"function"`;
   - нет `println!` / вывода в `stdout`;
   - все внешние команды запускаются через `/bin/sh` с явным `.env("PATH", ...)`;
   - в `init` и `command` нет блокирующих операций дольше 1 с (долгие задачи в `tokio::spawn`);
   - `configure()` обновляет живое состояние без перезапуска;
   - при сбое возвращается `Err(...)`, а не `Ok("ошибка...")`;
   - `Plugin::request` компилируется без ошибок лайфтайма (`let method = method.to_owned();`);
   - `build.sh` / CI подставляет `"arch": "armv7"` в `jarvis-plugin-armv7.zip` и `"arch": "arm64"` в `jarvis-plugin-arm64.zip`.
4. **Инструкция для пользователя в конце ответа**:
   - напомни создать **новый публичный репозиторий (не форк!)** с именем `yajarvis-<id>`, добавить в **About -> Topics** темы `yajarvis-plugin` и `yajarvis-<category>`, и опубликовать обычный **GitHub Release** (не Draft / не Pre-release) с `jarvis-plugin.zip` или `jarvis-plugin-arm64.zip` / `jarvis-plugin-armv7.zip`.
