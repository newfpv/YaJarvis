# Инструкция для ИИ-агентов (`AGENTS.md`)

> **Как использовать этот файл:**
> - **Claude Code / Codex / Cursor / Antigravity** читают `AGENTS.md` в корне репозитория автоматически.
> - **В ChatGPT / Claude / Gemini или для колонки без клона репозитория:** отправьте агенту ссылку:
>   `Прочитай https://raw.githubusercontent.com/newfpv/YaJarvis/main/AGENTS.md и собери диагностику с колонки для баг-репорта.`

Этот документ задаёт правила безопасного сбора диагностики, оформления баг-репортов и работы с кодом **YaJarvis**.

---

## 1. Безопасность — жёсткий запрет на утечку секретов (NEVER)

Перед передачей любых файлов пользователю, в Telegram-чат, в GitHub Issue или в Pull Request ты **ОБЯЗАН** защитить приватные данные владельца колонки:

1. **NEVER** читай, цитируй целиком, прикладывай или коммить следующие файлы с колонки:
   - `/data/opt/jarvisd/settings.json`
   - `/data/opt/jarvisd/seed.json`
   - `/data/opt/jarvisd/chatgpt-chat.json` и архивы чатов
   - cookies / файлы авторизации YouTube, Xiaomi, Smart Life (Tuya), Google Drive
2. **NEVER** публикуй в открытом виде:
   - API-ключи (`AIza...`, `sk-...`, `xai-...`, ключи ElevenLabs);
   - ссылки VPN (`vless://...`, `vmess://...`, `trojan://...`, `ss://...`) и IP-адреса личных VPN-серверов;
   - заголовки `Authorization`, `Bearer ...`, `Cookie`, JWT-токены (`eyJ...`), пароли панели и токены Умного дома / Яндекса (`yandex_token`, `home_assistant.token`);
   - email-адреса и содержимое личных промптов/памяти.
3. **MUST:** даже при чтении логов с колонки проверь вывод и замени любые найденные токены/ключи/URL с учётными данными на `[REDACTED]`.

---

## 2. Сбор полного и безопасного комплекта диагностики при баге

### Вариант А — готовый безопасный отчёт из веб-панели (предпочтительно)

В веб-панели колонки (`http://<IP-КОЛОНКИ>:8080` → вкладка **«Логи»**) есть кнопка **«Отчёт для баг-репорта»** (`GET /api/logs/report`). Она формирует `.txt`-файл с информацией о системе, безопасной сводкой настроек (без ключей и токенов), состоянием плагинов и хвостом логов `jarvisd.log`, `maind.log`, `mediad.log` с автоматической очисткой секретов.

### Вариант Б — сбор по ADB / SSH (только read-only команды)

Если панель недоступна, попроси разрешение на доступ к колонке (`adb -s <IP>:5555 shell ...` или `ssh`) и выполни **только** безопасные команды чтения:

```sh
# 1. Модель колонки, архитектура, версия YaJarvis
cat /proc/device-tree/model 2>/dev/null; echo
uname -a
/data/opt/jarvisd/jarvisd --version 2>/dev/null

# 2. Состояние процессов, памяти, диска и аудиоустройств
ps
free
df -h /data /tmp
aplay -l 2>/dev/null
arecord -l 2>/dev/null
ls -la /data/opt/jarvisd/plugins 2>/dev/null

# 3. Хвост журнала Джарвиса (перед отправкой проверь на отсутствие секретов!)
tail -n 300 /tmp/jarvisd/jarvisd.log 2>/dev/null || tail -n 300 /data/opt/jarvisd/jarvisd.log 2>/dev/null
```

При проблемах с передачей команд Алисе, обрывах звука, тишине или конфликтах воспроизведения дополнительно сними хвост логов штатной Алисы/медиа:

```sh
tail -n 120 /data/quasar/daemons_logs/maind.log 2>/dev/null
tail -n 120 /data/quasar/daemons_logs/mediad.log 2>/dev/null
```

---

## 3. Оформление GitHub Issue (без дублей и спама)

Перед созданием Issue выполни следующие шаги:

1. **Проверь, нет ли уже ответа или открытого тикета:**
   - просмотри [`docs/FAQ.md`](docs/FAQ.md) и [`README.md`](README.md);
   - проверь открытые и недавно закрытые Issues в `newfpv/YaJarvis`. Если похожая проблема уже открыта — не создавай дубль, лучше дополни существующий Issue новыми логами.
2. **Подготовь структурированный черновик по форме [`.github/ISSUE_TEMPLATE/bug_report.yml`](.github/ISSUE_TEMPLATE/bug_report.yml):**
   - точная модель колонки и версия YaJarvis;
   - используемый голосовой движок / режим (`Gemini Live`, `ChatGPT`, `OpenAI`, `Grok`, `ElevenLabs`, `Classic`) или проблемный плагин;
   - чёткие шаги воспроизведения + что ожидалось и что произошло;
   - очищенный от секретов диагностический отчёт (`yajarvis-diag-*.txt`) или хвост логов.
3. **MUST:** перед отправкой Issue или комментария на GitHub **покажи готовый текст пользователю на подтверждение**, чтобы исключить утечку приватных данных и дубликаты.

---

## 4. Правила для Pull Requests и разработки кода

1. **Где активный код:**
   - Ядро ассистента и веб-панель (API): Rust-воркспейс [`jarvis/Cargo.toml`](jarvis/Cargo.toml) (`jarvis/core`, `jarvis/dsp`, `jarvis/station-runtime`, `jarvis/official-plugin`, `jarvis/modules/*`).
   - Веб-интерфейс панели: SvelteKit в `jarvisd/ui`.
   - **НЕ правь** legacy Go-код в корне `jarvisd/` (кроме `jarvisd/ui`), если задача явно не касается старого установщика/миграций.
   - Для разработки отдельных плагинов строго следуй спецификации [`docs/PLUGIN-AGENT.md`](docs/PLUGIN-AGENT.md).
2. **Обязательная проверка перед PR:**
   - Собери панель (`jarvis/core` встраивает `build/ui/panel` через `jarvis/embed_ui.rs`):
     ```sh
     npm ci && npm run build:panel
     ```
   - Прогони тесты ядра и плагина:
     ```sh
     cargo test --manifest-path jarvis/Cargo.toml -p jarvis
     cargo test --manifest-path jarvis/Cargo.toml -p jarvis-official-plugin --features training
     ```
   - Делай изменения минимальными и по делу; не коммить `settings.json`, логи, записи голоса и временные артефакты.
