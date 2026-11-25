# Вики по плагинам YaJarvis

Подробное руководство по архитектуре расширений YaJarvis (начиная с `0.10.x`), каталогу официальных пакетов, установке, формату манифеста `plugin.json`, полному Lua 5.4 API, созданию Rust-пакетов и публикации в общий каталог GitHub.

> **Пишете плагин с помощью нейросети (ChatGPT, Claude, Cursor, Codex, Gemini)?**
> Мы подготовили специальную концентрированную спецификацию для AI-агентов: **[PLUGIN-AGENT.md](PLUGIN-AGENT.md)** (все контракты, жёсткие запреты, готовые шаблоны Lua/Rust и GitHub Actions CI + чеклист самопроверки).
> Просто отправьте ИИ ссылку:
> `Прочитай https://raw.githubusercontent.com/newfpv/YaJarvis/main/docs/PLUGIN-AGENT.md и напиши плагин YaJarvis для <ваша идея>.`
> Или скопируйте `docs/PLUGIN-AGENT.md` в корень своего репозитория плагина как `AGENTS.md` / `CLAUDE.md`.

## Содержание

- [1. Архитектура: ядро, Lua и Rust](#1-архитектура-ядро-lua-и-rust)
- [2. Официальный каталог плагинов](#2-официальный-каталог-плагинов)
- [3. Установка, обновление и безопасность (для пользователей)](#3-установка-обновление-и-безопасность-для-пользователей)
- [4. Справочник манифеста `plugin.json`](#4-справочник-манифеста-pluginjson)
  - [Поля манифеста](#поля-манифеста)
  - [Категории каталога и темы GitHub](#категории-каталога-и-темы-github)
  - [Типы полей настроек (`settings`)](#типы-полей-настроек-settings)
  - [Ограничения ZIP-архива](#ограничения-zip-архива)
- [5. Разработка Lua-плагинов (`kind: "lua"`)](#5-разработка-lua-плагинов-kind-lua)
  - [5.1. Песочница и точные лимиты](#51-песочница-и-точные-лимиты)
  - [5.2. Возвращаемая таблица и жизненный цикл](#52-возвращаемая-таблица-и-жизненный-цикл)
  - [5.3. Полный справочник API `jarvis.*`](#53-полный-справочник-api-jarvis)
  - [5.4. Быстрые команды и инструменты LLM (`command` и `tools`)](#54-быстрые-команды-и-инструменты-llm-command-и-tools)
  - [5.5. Своя текстовая модель (`provides: ["llm"]`)](#55-своя-текстовая-модель-provides--llm)
  - [5.6. Свои распознавание и озвучка (`provides: ["stt"]` и `["tts"]`)](#56-свои-распознавание-и-озвучка-provides-stt-и-tts)
  - [5.7. Готовые шаблоны кода](#57-готовые-шаблоны-кода)
- [6. Разработка Rust-плагинов (`kind: "rust"` и `kind: "module"`)](#6-разработка-rust-плагинов-kind-rust-и-kind-module)
  - [6.1. Минимальный рабочий шаблон](#61-минимальный-рабочий-шаблон)
  - [6.2. Протокол RPC, форматы и таймауты](#62-протокол-rpc-форматы-и-таймауты)
  - [6.3. Частые грабли Rust-плагинов](#63-частые-грабли-rust-плагинов)
  - [6.4. Сборка `arm64` / `armv7` и упаковка ZIP](#64-сборка-arm64--armv7-и-упаковка-zip)
- [7. Публикация плагина в каталог GitHub](#7-публикация-плагина-в-каталог-github)
  - [Чеклист перед публикацией](#чеклист-перед-публикацией)
- [8. Отладка и частые ошибки](#8-отладка-и-частые-ошибки)

---

## 1. Архитектура: ядро, Lua и Rust

В YaJarvis предусмотрены три уровня расширений:

1. **Встроенные в ядро (`jarvisd`).** Базовый голосовой тракт и **5 голосовых/AI-провайдеров** (`chatgpt`, `openai`, `gemini`, `grok`, `elevenlabs`) скомпилированы прямо в ядро без отдельных процессов. Устанавливать их как плагины больше не нужно (и установка стороннего пакета с таким `id` отклоняется).
2. **Lua-плагины (`kind: "lua"`).** Лёгкие скрипты на **Lua 5.4**, запускаются в изолированной виртуальной машине прямо внутри `jarvisd`. Идеальны для команд, HTTP-интеграций, текстовых моделей (OpenRouter, Ollama и т.п.), STT/TTS-обёрток, справок, таймеров и уведомлений. Не зависят от архитектуры процессора (`arm64` / `armv7`).
3. **Rust-пакеты (`kind: "rust"`).** Отдельные скомпилированные бинарники под Linux (`aarch64-unknown-linux-musl` или `armv7-unknown-linux-musleabihf`), общающиеся с ядром через [`jarvis-plugin-sdk`](../jarvis/plugin-sdk/examples/process_echo.rs). Подходят для сложных протоколов, криптографии, стриминга, работы с сокетами или больших вычислений.

| Критерий | Lua (`kind: "lua"`) | Rust-пакет (`kind: "rust"`) |
| :--- | :--- | :--- |
| Изоляция | Строгая песочница (без ФС, ОС и произвольных хостов) | Отдельный процесс с правами службы `jarvisd` |
| Кросс-платформенность | Один `.zip` на все архитектуры (`arm64`, `armv7`) | Отдельный бинарник под `arm64` и `armv7` |
| Расход памяти и диска | До 12 МБ RAM на VM, скрипт до 512 КБ | Зависит от бинарника (обычно 1–4 МБ на флеше) |
| Сеть | Только разрешённые в `network_allow` домены через `jarvis.fetch` | Любые сетевые библиотеки Rust |
| Время ответа на вызов | 2 с CPU / 20–30 с wall-clock | Контролируется процессом |
| Переживает обновление | `jarvis.store` (`plugins-data/<id>.json`) | Папка `data/` и файл `data.json` в каталоге плагина |

> Также в ядре поддерживается режим **`kind: "module"`** для модулей, вкомпилированных в кастомную сборку ядра через `native-plugins`. Подробнее — в [NATIVE-PLUGINS.md](NATIVE-PLUGINS.md).

---

## 2. Официальный каталог плагинов

Официальные пакеты публикуются в репозитории [`newfpv/YaJarvis`](https://github.com/newfpv/YaJarvis) (список — в [`plugins/index.json`](../plugins/index.json)). Колонка выбирает нужный формат сама: Lua-пакеты загружаются прямо из ветки/тега репозитория, а Rust-пакеты (`plugin-<id>-<arch>.zip`) — из GitHub Release с проверкой `SHA256SUMS`.

| ID | Название | Тип | Категория | Что делает | Что нужно для работы |
| :--- | :--- | :--- | :--- | :--- | :--- |
| `weather` | Погода | Lua | `info` | Текущая погода и прогноз на 3 дня через Open-Meteo + быстрые команды («какая погода», «нужен ли зонт») | **Без ключей** (достаточно указать город в настройках) |
| `currency` | Курсы валют | Lua | `info` | Курсы ЦБ РФ или Нацбанка Беларуси и пересчёт сумм («курс доллара», «сколько 100 евро») | **Без ключей** |
| `dice` | Монетка и кубик | Lua | `fun` | Орёл/решка, кубик, случайное число (рабочий пример простого плагина) | **Без ключей** и без сети |
| `yandexhome` | Яндекс Умный дом | Lua | `home` | Устройства, комнаты, группы, цвет, яркость и сценарии Яндекс Умного дома (`smart_home` + быстрые команды) | OAuth-токен Яндекс ID (`iot:view`, `iot:control`) |
| `homeassistant` | Home Assistant | Rust | `home` | Управление устройствами и сценариями Home Assistant, уборка роботом-пылесосом по комнатам | URL сервера HA и Long-Lived Access Token |
| `smartlife` | Smart Life | Rust | `home` | Устройства Tuya / Smart Life: прямое управление по локальной сети (LAN) и через облако (Zigbee) | Код пользователя Smart Life + QR-вход |
| `xiaomi` | Xiaomi Home | Rust | `home` | Устройства Xiaomi Home, свойства и действия MIoT | QR-вход в аккаунт Xiaomi |
| `youtube` | YouTube | Rust | `media` | Поиск, воспроизведение, очередь, рекомендации, повтор, история; автопауза Алисы | Без входа для большинства треков; код ТВ или cookies для истории/18+ |
| `vpn` | VPN | Rust | `system` | Выборочный SOCKS5/UDP-туннель VLESS (TCP/WS, TLS, REALITY `fp=chrome`, Vision) для выбранных сервисов | Ссылка `vless://...` |
| `training` | Обучение слова | Rust | `system` | Локальная смена фразы активации, сбор и разметка записей (до 100 МБ с резервом 5 МБ), экспорт ZIP с `finetune.py` | Включается в разделе «Обучение» |

---

## 3. Установка, обновление и безопасность (для пользователей)

Управлять пакетами можно в веб-панели: **Подключения → Плагины**.

- **Официальные и неофициальные.** В верхней части вкладки «Плагины» находится переключатель каталога:
  - **Официальные** — проверяются и сопровождаются автором проекта.
  - **Неофициальные (сообщество)** — открытые репозитории на GitHub с темой `yajarvis-plugin` (подгружаются топ-60 по звёздам). Перед первой установкой панель показывает предупреждение об ответственности.
- **Установка файлом `.zip`.** Нажмите загрузку архива, если хотите поставить собственный плагин или приватную сборку.
- **Включение, отключение и удаление.** Отключение останавливает VM или процесс плагина, но сохраняет настройки и данные. Удаление полностью стирает файлы и данные плагина, освобождая место на флешке.
- **Обновление и перезагрузка.** При обновлении пользовательские настройки в `plugins.json`, хранилище Lua (`plugins-data/<id>.json`) и папка `data/` Rust-плагина **сохраняются**. Если в `plugin.json` указано `"reboot_on_update": true`, при обновлении уже включённого плагина колонка выполнит перезагрузку (при пакетном обновлении — один раз в конце).
- **Место на флешке.** Перед распаковкой колонка проверяет свободное место — должно оставаться минимум `(размер_плагина_в_МБ + 3) МБ`.

**Безопасность:**
- **Lua-плагины** изолированы от файловой системы и системных команд и не могут обращаться ни к каким доменам, кроме перечисленных в `network_allow`.
- **Rust-плагины (`kind: "rust"`)** запускаются отдельным нативным процессом с правами `jarvisd`. Устанавливайте бинарные Rust-пакеты только из источников, которым доверяете.

---

## 4. Справочник манифеста `plugin.json`

`plugin.json` обязательно должен лежать либо в корне ZIP-архива, либо на один уровень ниже в единственной корневой папке (так GitHub упаковывает архивы исходников).

### Поля манифеста

| Поле | Тип | Обязательно | Описание и правила |
| :--- | :--- | :--- | :--- |
| `id` | `string` | **Да** | Латиница в нижнем регистре, цифры и дефис (`[a-z0-9-]`), первый символ — буква или цифра, длина **2–32** символа. Официальные ID (`chatgpt`, `openai`, `gemini`, `grok`, `elevenlabs`, `homeassistant`, `yandexhome`, `smartlife`, `xiaomi`, `vpn`, `youtube`, `training`) зарезервированы. |
| `name` | `string` | **Да** | Отображаемое название плагина (непустая строка). |
| `version` | `string` | **Да** | Семантическая версия, например `"1.0.0"` (используется при проверке обновлений). |
| `description` | `string` | Рекомендуется | Короткое описание возможностей для карточки в каталоге и настройках. |
| `author` | `string` | Рекомендуется | Имя или ник автора. |
| `homepage` | `string` | Опционально | Ссылка на репозиторий или документацию. |
| `kind` | `string` | **Да** | `"lua"` или `"rust"` (также `"module"` для встроенных в ядро сборок). Устаревшие `"js"` и `"native"` больше не поддерживаются. |
| `main` | `string` | **Да** | Имя входного файла внутри архива (например, `"main.lua"` для Lua или `"my-plugin"` для Rust). Для Rust: строго имя файла без `/` и не зарезервированное (`data`, `data.json`, `plugin.json`, `.`). При распаковке ядро само ставит права `0700` на `main`, а остальные файлы архива получают `0644` (скрипты рядом запускайте через `/bin/sh`). |
| `arch` | `string` | Для `rust` | Целевая архитектура: `"arm64"` или `"armv7"`. При установке ядро проверяет реальный ELF-заголовок бинарника! В `jarvis-plugin-armv7.zip` в `plugin.json` обязательно должно быть `"arch": "armv7"`. |
| `api` | `integer` | **Да** | Версия API плагинов. Сейчас актуальна **`1`**. |
| `category` | `string` | **Да** | Одна из 9 допустимых категорий (см. таблицу ниже). |
| `icon` | `string` | Опционально | Имя встроенной иконки панели. Доступные имена: `puzzle`, `shield`, `cloud`, `wifi`, `home`, `bolt`, `music`, `radio`, `mic`, `speaker`, `voice`, `moon`, `heart`, `search`, `tune`, `link`, `list`, `refresh`, `play`, `done`. Любое другое имя молча заменится на `puzzle`. |
| `provides` | `string[]` | Опционально | Возможности плагина: `"tools"`, `"commands"`, `"llm"`, `"stt"`, `"tts"`. (Значения `"vpn"` и ID официальных систем умного дома разрешены только официальным плагинам.) |
| `permissions` | `string[]` | Для Lua | Запрашиваемые права: `"tools"`, `"commands"`, `"say"`, `"network"`, `"storage"`, `"timers"`. |
| `network_allow` | `string[]` | Если есть `network` | Белый список доменов/origin для `jarvis.fetch`: точный хост (`api.example.com`), origin (`https://api.example.com:8443`) или поддомены (`*.example.com`). |
| `settings` | `Setting[]` | Опционально | Описание формы настроек, которую панель отрисует автоматически. |
| `min_core` | `string` | Опционально | Минимальная версия ядра YaJarvis, например `"0.10.5"`. На более старом ядре установка вернёт просьбу обновиться. |
| `reboot_on_update` | `boolean` | Опционально | По умолчанию `false`. Если `true`, при обновлении уже включённого плагина колонка перезагрузится. |

### Категории каталога и темы GitHub

Для категоризации в каталоге сообщества добавьте к репозиторию главную тему **`yajarvis-plugin`** и одну из тем категории `yajarvis-<id>`:

| `category` | Тема на GitHub | Раздел в панели | Примеры плагинов |
| :--- | :--- | :--- | :--- |
| `assistant` | `yajarvis-assistant` | Ассистенты и голоса | голосовые движки, LLM, STT/TTS |
| `home` | `yajarvis-home` | Умный дом | свет, розетки, датчики, сценарии |
| `media` | `yajarvis-media` | Музыка и видео | радио, подкасты, стримы, видео |
| `info` | `yajarvis-info` | Погода и справки | погода, курсы, новости, транспорт |
| `productivity` | `yajarvis-productivity` | Дела и заметки | списки, напоминания, календари, почта |
| `dev` | `yajarvis-dev` | Кодинг и IT | серверы, GitHub, мониторинг, сеть |
| `fun` | `yajarvis-fun` | Игры и развлечения | игры, шутки, викторины, случайности |
| `system` | `yajarvis-system` | Колонка и система | подсветка, кнопки, слово активации, VPN |
| `other` | `yajarvis-other` | Другое | всё остальное |

### Типы полей настроек (`settings`)

Каждое поле в массиве `settings` имеет вид:

```json
{
  "key": "api_key",
  "label": "API-ключ",
  "type": "password",
  "hint": "Подсказка при наведении на ?",
  "default": "",
  "required": true
}
```

Доступные значения `type`:

| `type` | Как отображается в панели | Особенности |
| :--- | :--- | :--- |
| `text` | Однострочное текстовое поле | По умолчанию `default` — строка |
| `password` | Поле скрытого ввода | В панели текущее значение маскируется (`задан ••••`), пустая отправка сохраняет прежнее |
| `bool` | Переключатель (switch) | `true` / `false` |
| `number` | Числовое поле | Передаётся как `number` или `null` |
| `select` | Выпадающий список | Обязательно укажите массив `"options": ["Вариант 1", "Вариант 2"]` |
| `textarea` | Многострочное поле (4 строки) | Для длинных промптов, JSON-списков и т.п. |

### Ограничения ZIP-архива

- Максимальный размер `.zip`: **24 МБ** (`ZIP_MAX`).
- Максимальный распакованный размер: **48 МБ** (`UNPACK_MAX`).
- Максимальный размер `plugin.json`: **256 КБ**.
- Запрещены символические ссылки, спецфайлы, абсолютные пути и сегменты `..`. Служебные каталоги `__MACOSX` игнорируются.

---

## 5. Разработка Lua-плагинов (`kind: "lua"`)

### 5.1. Песочница и точные лимиты

- **Движок:** Lua 5.4, по одной выделенной VM и рабочему потоку на каждый плагин.
- **Доступные стандартные библиотеки:** `table`, `string`, `math`, `utf8` и базовые функции Lua.
- **Запрещено и удалено:** `dofile`, `loadfile`, `load`, `collectgarbage`, `os`, `io`, `debug`, `package`/`require`, `coroutine`, загрузка бинарного байткода.
- **Лимиты исполнения:**
  - Максимальный размер `main.lua`: **512 КБ**.
  - Лимит памяти Lua VM: **12 МБ**.
  - Очередь вызовов плагина: **16 запросов**.
  - Лимит чистого вычисления (инструкции Lua): **2 секунды** на вызов.
  - Общее время выполнения (с учётом HTTP): **20 секунд** для обычных методов и таймеров, **25 секунд** для `service`, **30 секунд** для `transcribe` (STT) и `speak` (TTS).

### 5.2. Возвращаемая таблица и жизненный цикл

Файл `main.lua` должен вернуть таблицу с нужными экспортами (все поля необязательны, если не заявлены в `provides`):

```lua
return {
  init = function()
    jarvis.status("Готов")
  end,

  configure = function(settings)
    -- Вызывается после сохранения настроек в панели (jarvis.settings уже обновлён)
  end,

  command = function(text)
    -- Нужны permission "commands". Возврат строки или { handled = true, answer = "..." }
  end,

  tools = {
    {
      name = "my_tool",
      description = "Что делает инструмент и когда модели его вызывать",
      parameters = {
        type = "object",
        properties = {
          query = { type = "string", description = "Аргумент" },
        },
        required = { "query" },
      },
      handler = function(args)
        return "Результат работы инструмента"
      end,
    },
  },

  -- Опционально (если заявлены соответствующие provides):
  chat = function(request) ... end,        -- provides: ["llm"]
  transcribe = function(request) ... end,  -- provides: ["stt"]
  speak = function(request) ... end,       -- provides: ["tts"]
  voices = function() ... end,             -- опционально для provides: ["tts"]
}
```

### 5.3. Полный справочник API `jarvis.*`

| Символ | Права | Описание |
| :--- | :--- | :--- |
| `jarvis.settings` | всегда | Таблица текущих настроек из манифеста с подставленными `default`. Перед `configure(settings)` обновляется автоматически. |
| `jarvis.info` | всегда | Сведения о колонке: `{ city, timezone, room, language, model, assistant }`. |
| `jarvis.log(text)` | всегда | Пишет строку (до 600 символов) в журнал колонки с префиксом `плагин <id>:`. |
| `jarvis.status(text)` | всегда | Короткий статус (до 200 символов), отображается в карточке плагина в панели. |
| `jarvis.json.encode(val)` | всегда | Сериализация таблицы/значения Lua в JSON-строку. |
| `jarvis.json.decode(str)` | всегда | Разбор JSON-строки в значение Lua. |
| `jarvis.now()` | всегда | Текущее Unix-время в миллисекундах (integer). |
| `jarvis.normalize(text)` | всегда | Канонизация текста как для `command`: нижний регистр, `ё → е`, без пунктуации, без ведущего `джарвис` / `jarvis`. |
| `jarvis.norm(text)` | всегда | Нижний регистр, `ё → е`, без пунктуации, одиночные пробелы (без удаления `джарвис`). |
| `jarvis.urlencode(text)` | всегда | URL-кодирование строки для query-параметров. |
| `jarvis.random(min, max)` | всегда | Криптографически стойкое целое число в `[min, max]`. |
| `jarvis.say(text)` | `"say"` | Озвучивает фразу (до 600 символов) текущим голосом ассистента на колонке. |
| `jarvis.store.get(key)` | `"storage"` | Читает значение по ключу из `plugins-data/<id>.json` (`nil`, если ключа нет). |
| `jarvis.store.set(key, val)` | `"storage"` | Сохраняет JSON-значение по ключу (`1–256` байт). Передача `nil` удаляет ключ. Общий размер хранилища ≤ **256 КБ**, запись атомарная. |
| `jarvis.timers.after(ms, fn)` | `"timers"` | Одноразовый таймер через `ms` мс (до 7 суток). Возвращает числовой `id`. |
| `jarvis.timers.every(ms, fn)` | `"timers"` | Повторяющийся таймер с периодом от **100 мс** до 7 суток. Максимум **64** активных таймера на плагин. |
| `jarvis.timers.cancel(id)` | `"timers"` | Отменяет таймер по `id`, возвращает `boolean`. |
| `jarvis.fetch(url, opts)` | `"network"` + `network_allow` | Синхронный HTTP/HTTPS-запрос с изоляцией доменов (подробности ниже). |

#### Детали `jarvis.fetch(url, options)`

- Возвращает таблицу `{ status = <number>, ok = <boolean>, body = <string> }` (или `body_base64`, если задан `response_encoding = "base64"`).
- Допустимые поля `options`:
  - `method` — HTTP-метод (`"GET"`, `"POST"`, `"PUT"`, `"DELETE"` и т.д.).
  - `headers` — таблица строковых заголовков `{ ["Authorization"] = "Bearer ..." }`.
  - `timeout_ms` — таймаут запроса в миллисекундах (зажимается в диапазон **1000–25000 мс**).
  - **Тело запроса (только одно из трёх):**
    - `body` — строка UTF-8 до **1 МБ**.
    - `body_base64` — бинарные данные в Base64 (до **3 МБ** после декодирования).
    - `multipart` — массив частей `{ name = "...", value = "..." }` или `{ name = "...", filename = "...", content_type = "...", data_base64 = "..." }` (до **4 МБ**; `Content-Type: multipart/form-data; boundary=...` ставится сам).
  - `response_encoding` — `"utf8"` (по умолчанию, ответ до **1 МБ**) или `"base64"` (ответ до **4 МБ**).
- **Безопасность и маршрутизация:**
  - Проверяется каждый HTTP-редирект (до 5 переходов, только для `GET`/`HEAD`); запрещено понижение `HTTPS → HTTP`; при редиректе на другой origin запрос с пользовательскими `headers` отклоняется; URL с `user:pass@` запрещены.
  - В `network_allow` шаблон `*.example.com` разрешает только поддомены и **не включает** корневой `example.com` (если нужны оба, укажите оба).
  - **Обработка ошибок:** HTTP-ответы `4xx`/`5xx` возвращаются в таблице (`res.ok == false`), а сетевые ошибки, таймаут, запрещённый домен и сбой VPN бросают Lua-ошибку. Оборачивайте `jarvis.fetch(...)` и `jarvis.json.decode(...)` в `pcall`, чтобы не ронять обработчик.
  - Трафик идёт по настройке VPN для **«Плагины»**. Если VPN включён, но туннель не работает, прямой запрос **не выполняется** — возвращается ошибка.

#### Частые грабли Lua-плагинов

- **Один файл без `require`:** модули `package`, `require`, `os`, `io`, `debug`, `coroutine` удалены. Весь код держите в `main.lua` (до 512 КБ), а вместо `os.time()` используйте `math.floor(jarvis.now() / 1000)`.
- **Кириллица и регистр:** стандартный `string.lower()` не работает с русскими буквами в UTF-8. Всегда используйте `jarvis.norm(s)` или `jarvis.normalize(s)`.
- **Строгое соответствие `provides` и `permissions`:** если экспортируете `chat`, `transcribe` или `speak`, обязательно укажите `llm`, `stt` или `tts` в `provides` (и наоборот, иначе плагин не запустится). Для `tools` нужно право `"tools"`, для `command` нужно `"commands"`.
- **Не перехватывайте чужие фразы в `command(text)`:** избегайте слишком широкого поиска подстроки вроде `text:find("погода")` (он перехватит «переведи слово погода»). Если команда не ваша, возвращайте `nil`.
- **Короткий текст для озвучки:** ответы `command` и `tools` идут в TTS. Не возвращайте Markdown-разметку, ссылки и эмодзи.

### 5.4. Быстрые команды и инструменты LLM (`command` и `tools`)

- **`command(text)`** (требуется `"commands"` в `permissions`):
  - Получает канонизированную фразу без «Джарвис», в нижнем регистре и без знаков препинания.
  - Если плагин понял фразу — верните строку ответа (например, `"Готово"` или `{ handled = true, answer = "Готово" }`). Ответ сразу озвучится **без обращения к нейросети** (быстро и 0 токенов).
  - Если фраза не ваша — верните `nil`, и она пойдёт дальше по цепочке другим плагинам или в LLM.
- **`tools`** (требуется `"tools"` в `permissions`):
  - Максимум **64 инструмента** на плагин. Имя инструмента: `1–64` символа (`[a-zA-Z0-9_-]`).
  - Нельзя перекрывать встроенные инструменты ядра (`CORE_TOOLS`): `smart_home`, `play_music`, `music_control`, `alice_command`, `recall_conversation`, `edit_memory`, `home_assistant`, `xiaomi_home`, `smart_life`.
  - `handler(args)` получает таблицу распарсенных JSON-аргументов и возвращает строку (или таблицу, которая сериализуется в ответ модели).

### 5.5. Своя текстовая модель (`provides: ["llm"]`)

Чтобы плагин появился в выборе «Текстовая модель (LLM)» в классическом режиме:

1. Укажите в `plugin.json`: `"provides": ["llm"]` и `"min_core": "0.10.5"` (или `"0.9.84"`).
2. Экспортируйте функцию `chat(request)`.

**Что приходит в `request`:**
- `request.messages` — массив `{ role = "system" | "user" | "assistant" | "tool", content = "..." }` (включая инструкции профиля, историю текущей голосовой сессии, вопрос пользователя и промежуточные результаты `tool` / `assistant.tool_calls` в рамках одного хода).
- `request.model` — строка с именем модели из профиля ассистента (если пользователь указал её в панели; иначе `""`).
- `request.tools` — массив определений инструментов в формате OpenAI Chat Completions (`{ type = "function", ["function"] = { name, description, parameters } }`).

**Что может вернуть `chat(request)`:**
- Строку — финальный текстовый ответ пользователю.
- Таблицу с вызовом инструментов:
  ```lua
  return {
    text = "",
    tool_calls = {
      { id = "call_1", name = "get_weather", arguments = '{"city":"Москва"}' }
    }
  }
  ```
  Ядро проверит имена по выданному списку, выполнит инструменты и снова вызовет `chat` с результатами (до **5 итераций** `chat` и до **16 инструментов** за один ход).

Полный готовый пример: [`plugins/examples/lua-openrouter/`](../plugins/examples/lua-openrouter).

### 5.6. Свои распознавание и озвучка (`provides: ["stt"]` и `["tts"]`)

В классическом голосовом пайплайне можно заменить штатные `STT` и `TTS` на свои Lua-плагины:

- **Свой STT (`"provides": ["stt"]`):**
  Экспортируйте `transcribe(request)`:
  - Вход `request`: `{ audio_b64 = "<WAV в base64>", format = "wav", rate = 16000, model = "...", language = "ru" }`.
  - Выход: `{ text = "распознанный текст" }` (или просто строку).
  - Таймаут выполнения: **30 секунд**.

- **Свой TTS (`"provides": ["tts"]`):**
  Экспортируйте `speak(request)` (и опционально `voices()`):
  - Вход `request`: `{ text = "что сказать", voice = "выбранный ID", model = "...", language = "ru" }`.
  - Выход: `{ audio = "<PCM16 mono LE в base64>", rate = 24000 }`. Поддерживаемые частоты: **`16000`**, **`24000`**, **`48000`** Гц (моно, signed 16-bit little-endian PCM).
  - Опциональная функция `voices()` возвращает массив `{ { id = "v1", name = "Алёна", note = "спокойный" }, ... }` — тогда панель покажет выпадающий список голосов в профиле ассистента.

### 5.7. Готовые шаблоны кода

#### Шаблон 1: Быстрая команда + инструмент + HTTP + кеш в `jarvis.store`

`plugin.json`:
```json
{
  "id": "cat-facts",
  "name": "Факты о котах",
  "version": "1.0.0",
  "description": "Случайный факт и быстрая команда «факт о котах».",
  "author": "Ваше Имя",
  "kind": "lua",
  "main": "main.lua",
  "api": 1,
  "category": "fun",
  "icon": "puzzle",
  "provides": ["tools", "commands"],
  "permissions": ["tools", "commands", "network", "storage"],
  "network_allow": ["catfact.ninja"],
  "settings": [
    { "key": "max_len", "label": "Макс. длина ответа", "type": "number", "default": 160 }
  ]
}
```

`main.lua`:
```lua
local function fetch_fact()
  local limit = tonumber(jarvis.settings.max_len) or 160
  local res = jarvis.fetch("https://catfact.ninja/fact?max_length=" .. limit, { timeout_ms = 5000 })
  assert(res.ok, "Сервис вернул HTTP " .. tostring(res.status))
  local data = jarvis.json.decode(res.body)
  jarvis.store.set("last_fact", data.fact)
  return data.fact
end

return {
  init = function()
    local prev = jarvis.store.get("last_fact")
    jarvis.status(prev and "Кеш готов" or "Готов к работе")
  end,
  tools = {{
    name = "get_cat_fact",
    description = "Возвращает случайный интересный факт о кошках.",
    parameters = { type = "object", properties = {} },
    handler = function() return fetch_fact() end,
  }},
  command = function(text)
    if text == "факт о котах" or text == "расскажи факт о котах" then
      return fetch_fact()
    end
  end,
}
```

#### Шаблон 2: Подключение OpenAI-совместимой или локальной LLM (Ollama / OpenRouter / Groq)

Полный рабочий плагин OpenRouter лежит в [`plugins/examples/lua-openrouter/`](../plugins/examples/lua-openrouter). Чтобы перенаправить его на локальный сервер **Ollama** в домашней сети, достаточно поменять `network_allow` на `["192.168.1.50"]` (или `["localhost"]`, если на той же машине) и URL `jarvis.fetch` на `http://192.168.1.50:11434/v1/chat/completions`.

#### Шаблон 3: Каркас STT + TTS плагина

```lua
return {
  voices = function()
    return {
      { id = "male", name = "Мужской", note = "чёткий" },
      { id = "female", name = "Женский", note = "мягкий" },
    }
  end,

  transcribe = function(req)
    local res = jarvis.fetch("https://stt.example.org/transcribe", {
      method = "POST",
      headers = { ["Authorization"] = "Bearer " .. (jarvis.settings.token or "") },
      multipart = {
        { name = "file", filename = "mic.wav", content_type = "audio/wav", data_base64 = req.audio_b64 },
        { name = "language", value = req.language or "ru" },
      },
    })
    assert(res.ok, "STT HTTP " .. tostring(res.status))
    local json = jarvis.json.decode(res.body)
    return { text = json.text or "" }
  end,

  speak = function(req)
    local res = jarvis.fetch("https://tts.example.org/speak", {
      method = "POST",
      headers = {
        ["Authorization"] = "Bearer " .. (jarvis.settings.token or ""),
        ["Content-Type"] = "application/json",
      },
      body = jarvis.json.encode({ text = req.text, voice = req.voice }),
      response_encoding = "base64",
    })
    assert(res.ok, "TTS HTTP " .. tostring(res.status))
    -- Если сервер отдал сырой PCM16 24кГц mono:
    return { audio = res.body_base64, rate = 24000 }
  end,
}
```

---

## 6. Разработка Rust-плагинов (`kind: "rust"` и `kind: "module"`)

Когда возможностей Lua недостаточно (нужны системные вызовы, бинарные протоколы, сокеты, криптография или скрипты на колонке), соберите нативный бинарный плагин с помощью крейта **`jarvis-plugin-sdk`** с feature `process`.

### 6.1. Минимальный рабочий шаблон

Подключите SDK в `Cargo.toml`:

```toml
[dependencies]
jarvis-plugin-sdk = { git = "https://github.com/newfpv/YaJarvis", package = "jarvis-plugin-sdk", features = ["process"] }
serde_json = "1"
tokio = { version = "1", features = ["rt", "macros", "process", "sync"] }
```

Полный рабочий каркас `src/main.rs` с настройками, быстрой командой, инструментом LLM, фоновой задачей и запуском системного скрипта:

```rust
use jarvis_plugin_sdk::{Map, Plugin, Response, Value, json, process};
use std::sync::{Arc, RwLock};
use tokio::process::Command;

// Окружение процесса очищено (env_clear), поэтому PATH задаём явно!
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

impl Plugin for MyPlugin {
    // ВАЖНО: формат ПЛОСКИЙ (без вложенного ключа "function"!)
    // tools() вызывается один раз при init.
    fn tools(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "name": "my_plugin_status",
            "description": "Показывает статус сервиса моего плагина",
            "parameters": {
                "type": "object",
                "properties": {
                    "verbose": { "type": "boolean", "description": "Подробный вывод" }
                }
            }
        })]
    }

    // Вызывается при сохранении настроек в панели (без перезапуска процесса)
    fn configure(&self, settings: Map<String, Value>) {
        if let Ok(mut st) = self.state.write() {
            *st = Self::from_settings(&settings);
        }
    }

    fn request(&self, method: &str, params: Value) -> Response<'_> {
        // Копируем method до Box::pin, иначе borrow checker ругнётся на lifetime &str
        let method = method.to_owned();
        let state = self.state.clone();

        Box::pin(async move {
            match method.as_str() {
                "command" => {
                    let text = params.as_str().unwrap_or("");
                    match text {
                        "статус сервиса" => Ok(json!("Сервис работает")),
                        "запусти долгую задачу" => {
                            // Таймаут command = 2 с! Долгие операции уносим в фон:
                            tokio::spawn(async move {
                                let out = Command::new("/bin/sh")
                                    .args(["-c", "echo ok"])
                                    .env("PATH", DEFAULT_PATH)
                                    .output()
                                    .await;
                                eprintln!("my-plugin: bg done: {:?}", out.map(|o| o.status));
                            });
                            Ok(json!("Запустил задачу в фоне"))
                        }
                        // Не наша фраза -> обязательно возвращаем Value::Null!
                        _ => Ok(Value::Null),
                    }
                }
                "tool" => {
                    let name = params["name"].as_str().unwrap_or("");
                    let _args = &params["arguments"];
                    match name {
                        "my_plugin_status" => {
                            let on = state.read().map(|s| s.enabled).unwrap_or(false);
                            Ok(json!(if on { "Включён" } else { "Выключен" }))
                        }
                        // При реальной ошибке возвращаем Err(...), а не Ok("ошибка...")
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
        let state = MyPlugin::from_settings(&init.settings);
        Ok(Box::new(MyPlugin {
            state: Arc::new(RwLock::new(state)),
        }))
    })
    .await
}
```

Минимальный пример с `llm.chat` также лежит в [`jarvis/plugin-sdk/examples/process_echo.rs`](../jarvis/plugin-sdk/examples/process_echo.rs).

### 6.2. Протокол RPC, форматы и таймауты

Ядро общается с процессом по `stdin`/`stdout` (JSON-фреймы до 4 МБ, вызовы приходят **последовательно**, очередь 8 запросов). Если любой вызов превысил свой таймаут или сломал фрейм, **ядро принудительно убивает процесс плагина**.

| Метод | Что приходит в `params` | Что вернуть | Таймаут |
| :--- | :--- | :--- | :--- |
| `init` | `Init { settings, info }` в замыкании `process::serve` | `Ok(Box::new(plugin))` или `Err("причина")` | **5 с** (при ошибке/таймауте установка откатывается!) |
| `configure` | `Map<String, Value>` в `Plugin::configure()` | ничего (обновите внутренний `RwLock`/`Mutex`) | **5 с** |
| `command` | JSON-строка с нормализованной фразой (нижний регистр, `ё -> е`, без пунктуации и без «джарвис») | `Ok(json!("Ответ"))` или `Ok(Value::Null)` если фраза не ваша | **2 с** |
| `tool` | `{"name": "<tool_name>", "arguments": { ... }}` | `Ok(json!("Результат"))` или `Err("описание ошибки")` | **15 с** |
| `llm.chat` | `{ messages, model, tools }` (как в Lua `chat`) | Строка или `{ text, tool_calls }` | **30 с** |
| `stt.transcribe` | `{ audio_b64, format, rate, model, language }` | `{ text: "..." }` или строка | **45 с** |
| `tts.speak` | `{ text, voice, model, language }` | `{ audio: "<base64 PCM16>", rate: 24000 }` | **45 с** |
| `shutdown` | обрабатывается SDK автоматически, вызывается `Plugin::stop()` | завершение процесса | **1 с** |

### 6.3. Частые грабли Rust-плагинов

Прочитайте эти 7 пунктов перед сборкой релиза. Почти все проблемы сторонних Rust-плагинов связаны именно с ними:

1. **Формат `Plugin::tools()` должен быть ПЛОСКИМ!**
   Ядро проверяет на верхнем уровне `tool["type"] == "function"`, валидность `tool["name"]` и `tool["parameters"].is_object()`.
   - ❌ **Неправильно** (формат OpenAI Chat Completions, ядро **молча отбросит все инструменты**!):
     ```json
     { "type": "function", "function": { "name": "my_tool", "description": "...", "parameters": { "type": "object", "properties": {} } } }
     ```
   - ✅ **Правильно** (плоский формат):
     ```json
     { "type": "function", "name": "my_tool", "description": "...", "parameters": { "type": "object", "properties": {} } }
     ```
   Имя инструмента: `[a-zA-Z0-9_-]`, длина 1..64, нельзя перекрывать встроенные `CORE_TOOLS`. Максимум 64 инструмента.

2. **Процесс запускается с пустым окружением (`env_clear()`).**
   Переменные `PATH`, `HOME`, `TMPDIR` у вашего плагина **пустые**. Вызов `Command::new("sh")` или использование `pgrep`, `pidof`, `iptables`, `curl` внутри шелл-скриптов упадёт с `No such file or directory` / `not found`.
   Всегда запускайте `/bin/sh` по полному пути и передавайте `.env("PATH", "/opt/sbin:/opt/bin:/usr/sbin:/usr/bin:/sbin:/bin")`.

3. **Жёсткий таймаут 2 секунды на `command` (и 5 секунд на `init`).**
   Нельзя синхронно скачивать файлы, ставить пакеты или запускать долгие скрипты внутри `init`, `command` или `tool`: ядро убьёт процесс по таймауту. Используйте асинхронный `tokio::process::Command` (а не блокирующий `std::process::Command`) и выносите долгие операции в `tokio::spawn`, сразу возвращая короткий ответ.

4. **`stdout` зарезервирован под бинарный RPC-протокол.**
   Любой `println!` (или дочерний процесс с унаследованным `stdout`) сломает обмен и приведёт к `Rust-плагин не подтвердил запуск`. Все логи пишите строго в **`stderr`** (`eprintln!`, `log`), они попадут в системный журнал колонки.

5. **Возвращайте `Err(...)` при сбое.**
   Если инструмент или скрипт завершился с ненулевым кодом, возвращайте `Err("текст ошибки".into())` (ядро обрежет до 300 символов и передаст ошибку модели/панели), а не `Ok(json!("ошибка ..."))`.

6. **Рабочая папка, пути и права файлов.**
   - Текущая директория процесса (`std::env::current_dir()`) при запуске: `/data/opt/jarvisd/plugins/<id>/data`.
   - Всё внутри `data/` и файл `../data.json` **сохраняются при обновлении плагина**.
   - Остальные файлы из вашего ZIP лежат на уровень выше (`current_dir().parent()`), перезаписываются при обновлении и имеют права `0644` (не исполняемые; права `0700` ядро выдаёт только самому бинарнику `main`).

7. **Лайфтайм `method: &str` в `Plugin::request`.**
   Сигнатура `fn request(&self, method: &str, params: Value) -> Response<'_>` привязывает время жизни `Future` к `&self`, а не к `method`. Скопируйте `let method = method.to_owned();` (или разберите `match method`) **до** `Box::pin(async move { ... })`.

### 6.4. Сборка `arm64` / `armv7` и упаковка ZIP

Соберите статические бинарники под Linux `musl`:
- **ARM64** (Станция Лайт 1/2, Мини 2/3, Миди, Станция 2/3): `aarch64-unknown-linux-musl` -> `"arch": "arm64"`, файл релиза **`jarvis-plugin-arm64.zip`**.
- **ARMv7** (часть моделей Станции Мини 1 и 32-битные платы): `armv7-unknown-linux-musleabihf` -> `"arch": "armv7"`, файл релиза **`jarvis-plugin-armv7.zip`**.

**Важно:** в каждом архиве `plugin.json` должен содержать **свой** `"arch"`! Частая ошибка в `build.sh` это копировать один `plugin.json` с `"arch": "arm64"` в оба ZIP-архива, из-за чего установка на `armv7` падает на проверке ELF.

Пример правильного `build.sh` (через `cargo-zigbuild` или `cross`):

```sh
#!/bin/sh
set -eu

BIN="my-plugin"

pack() {
  target="$1"
  arch="$2"
  out="dist/$arch"
  zipname="jarvis-plugin-$arch.zip"

  cargo zigbuild --release --target "$target"
  rm -rf "$out"
  mkdir -p "$out"
  cp "target/$target/release/$BIN" "$out/$BIN"
  # Подставляем правильный arch именно для этого архива!
  sed "s/\"arch\"[[:space:]]*:[[:space:]]*\"[^\"]*\"/\"arch\": \"$arch\"/" plugin.json > "$out/plugin.json"

  rm -f "$zipname"
  (cd "$out" && zip -r "../../$zipname" .)
}

pack aarch64-unknown-linux-musl arm64
pack armv7-unknown-linux-musleabihf armv7
```

Подробности про встраивание `kind: "module"` в кастомную сборку ядра: **[NATIVE-PLUGINS.md](NATIVE-PLUGINS.md)**.

---

## 7. Публикация плагина в каталог GitHub

Чтобы любой пользователь YaJarvis видел ваш плагин во вкладке **Подключения -> Плагины -> Неофициальные** и мог поставить или обновить его в один клик:

1. **Создайте обычный публичный репозиторий на GitHub (НЕ форк!).**
   - Если вы делаете плагин на основе чужого репозитория, не жмите кнопку **Fork** на GitHub. Создайте новый чистый репозиторий и залейте код туда (укажите первоисточник в README).
   - Почему это важно: на прошивках `0.10.20` и ниже GitHub Search API по умолчанию **исключает форки** из выдачи по теме `yajarvis-plugin`, и плагин вообще не появится в каталоге. Начиная с `0.10.21` форки с явно добавленной темой тоже подхватываются, но самостоятельный репозиторий работает сразу у **всех** пользователей на любых версиях.
   - Репозиторий должен быть **публичным** и **не архивным** (`archived: false`).
   - Рекомендуемое имя репозитория: **`yajarvis-<id>`** (или `yajarvis-plugin-<id>`), где `<id>` совпадает с полем `id` в `plugin.json`. Тогда на всех версиях (включая `0.10.20` и ниже) карточка установленного плагина не задублируется с карточкой каталога.

2. **Добавьте темы (Topics) в настройках репозитория (About -> шестерёнка -> Topics):**
   - обязательная тема: **`yajarvis-plugin`**;
   - одна тема категории: `yajarvis-assistant`, `yajarvis-home`, `yajarvis-media`, `yajarvis-info`, `yajarvis-productivity`, `yajarvis-dev`, `yajarvis-fun`, `yajarvis-system` или `yajarvis-other`.

3. **Опубликуйте GitHub Release (Releases -> Draft a new release -> Publish release):**
   - Релиз должен быть **полноценным опубликованным релизом** (НЕ Draft и НЕ Pre-release, иначе API `/releases/latest` вернёт 404).
   - Прикрепите к релизу ZIP-архивы со **строго точными именами**:
     - для Lua-плагина: **`jarvis-plugin.zip`**;
     - для Rust-плагина: **`jarvis-plugin-arm64.zip`** и/или **`jarvis-plugin-armv7.zip`**.
   - Ядро ищет сначала `jarvis-plugin-<arch>.zip`, затем `jarvis-plugin.zip`. Любые другие имена (например `my-plugin-1.0.zip` или автоматический `Source code (zip)`) игнорируются.

4. **Кеш каталога:** колонка кеширует каталог сообщества на **1 час** (ошибки на 5 минут). Чтобы увидеть только что опубликованный плагин сразу, нажмите кнопку обновления каталога в панели.

5. **Обновления:** при выходе новой версии поднимите `version` в `plugin.json`, пересоберите ZIP и опубликуйте новый Release. Пользовательские настройки и папка `data/` сохранятся автоматически.

### Чеклист перед публикацией

- [ ] Репозиторий **публичный**, **не архивный** и **не форк**
- [ ] В **Topics** добавлены `yajarvis-plugin` и тема категории (`yajarvis-...`)
- [ ] Имя репозитория `yajarvis-<id>` совпадает с `id` в `plugin.json`
- [ ] Создан обычный (не Draft / не Pre-release) **GitHub Release** с ассетом `jarvis-plugin.zip` или `jarvis-plugin-arm64.zip` / `jarvis-plugin-armv7.zip`
- [ ] В `plugin.json` указана существующая иконка `icon` из списка панели и актуальная `version`
- [ ] Для Rust: в `jarvis-plugin-armv7.zip` внутри `plugin.json` стоит `"arch": "armv7"` (не `"arm64"`)
- [ ] Для Rust: в `tools()` используется **плоский** JSON без вложенного `"function"`, для внешних команд передаётся `PATH`, а долгие операции вынесены в `tokio::spawn`
- [ ] Плагин проверен локально на колонке через загрузку `.zip` в панели: нет ошибок в **Журнале**, быстрая команда отвечает, а инструменты видны модели

---

## 8. Отладка и частые ошибки

Смотреть логи работы плагина удобнее всего в разделе **Журнал** веб-панели (или по ADB в `/tmp/jarvisd/jarvisd.log`). Для отладки в Lua вызывайте `jarvis.log("...")` и `jarvis.status("...")`, в Rust пишите в `stderr` (`eprintln!`).

| Ошибка или симптом | Причина | Как исправить |
| :--- | :--- | :--- |
| Плагина нет в «Неофициальные (сообщество)» | Репозиторий сделан как **форк** (на `0.10.20` и ниже форки скрыты), не добавлена тема `yajarvis-plugin`, репо приватный/архивный или сработал кеш каталога (1 час) | Создайте самостоятельный публичный репозиторий (не форк), добавьте тему `yajarvis-plugin`, нажмите обновление каталога в панели |
| `в последнем релизе owner/repo нет jarvis-plugin-arm64.zip или jarvis-plugin.zip` | Нет опубликованного релиза (или он Draft / Pre-release), либо архив назван иначе | Опубликуйте обычный Release и прикрепите файл с точным именем `jarvis-plugin.zip` или `jarvis-plugin-arm64.zip` / `jarvis-plugin-armv7.zip` |
| Инструменты Rust-плагина не вызываются моделью | В `Plugin::tools()` использован вложенный ключ `"function": {...}`, либо имя инструмента нарушает `[a-zA-Z0-9_-]` / пересекается с `CORE_TOOLS` | Переведите `tools()` в плоский формат `{"type": "function", "name": "...", "description": "...", "parameters": {...}}` |
| `No such file or directory` / скрипты не находят `sh`, `iptables`, `curl` в Rust-плагине | Процесс запускается с `env_clear()` (пустой `PATH`), либо скрипт в ZIP имеет права `0644` | Вызывайте `/bin/sh` по полному пути и передавайте `.env("PATH", "/opt/sbin:/opt/bin:/usr/sbin:/usr/bin:/sbin:/bin")` |
| `Rust-плагин не ответил вовремя` / `Процесс Rust-пакета завершился` | Превышен таймаут RPC (`command` = 2 с, `init` = 5 с, `tool` = 15 с) или блокирующий вызов повесил цикл | Используйте `tokio::process::Command` и выносите долгую работу (установку, скачивание) в `tokio::spawn` |
| `Это пакет для старого ядра` | В `plugin.json` указано `kind: "js"` или `"native"` | Перепишите под `kind: "lua"` или `kind: "rust"` с `api: 1` |
| `Для Rust-пакета нужны подходящая архитектура, api: 1 и имя бинарника в корне ZIP` / `не подходит для этой архитектуры` | В `jarvis-plugin-armv7.zip` оставили `"arch": "arm64"`, неверный ELF-таргет, `api != 1` или в `main` указан слэш | Соберите под нужный `musl`-таргет, подставьте правильный `"arch"` в `plugin.json` каждого архива, положите бинарник в корень ZIP |
| `Rust-плагин не подтвердил запуск` | В Rust-плагине что-то выводится в `stdout` до или во время `init` | Перенесите все логи в `stderr` (`eprintln!`), не наследуйте `stdout` в дочерних процессах |
| `домен не разрешён в network_allow` | URL-запрос в `jarvis.fetch` ушёл на домен, которого нет в `network_allow` | Добавьте точный домен (или origin / `*.domain.tld`) в `network_allow` в `plugin.json` |
| `redirect с заголовками на другой домен` | Сработала защита от утечки `Authorization`-ключей при редиректе | Укажите конечный URL без редиректа либо не передавайте секретные заголовки при публичном GET |
| `Lua: вызов прерван или превышено время выполнения` | Скрипт крутился > 2 с CPU или завис на HTTP > 20 с (30 с для STT/TTS) | Уменьшите `timeout_ms` у `fetch`, перенесите долгую работу в Rust-пакет |
| `недопустимое или повторное имя инструмента` | Имя инструмента пересекается с `CORE_TOOLS` или содержит кириллицу/пробелы | Используйте уникальное имя `[a-zA-Z0-9_-]` длиной 1..64 символа |
| Иконка в панели всегда «пазл» | В `plugin.json` указано имя `icon`, которого нет в наборе панели | Выберите имя из списка в разделе 4 (например `shield`, `wifi`, `cloud`, `bolt`, `home`) |
| `мало места на колонке` | Не хватает свободного места на `/data` (требуется `размер + 3 МБ`) | Удалите неиспользуемые плагины или лишние записи в «Обучении» |
