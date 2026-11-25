<p align="center">
  <img src="docs/cover.svg" alt="YaJarvis — ваш голосовой помощник на Яндекс Станции" width="100%">
</p>

<h1 align="center">Джарвис на вашей Яндекс Станции</h1>

<p align="center">
  Живой разговор с AI, музыка с YouTube и управление умным домом прямо на колонке.<br>
  Локальное слово активации, несколько ассистентов, бесплатные и платные нейросети, плагины на Lua и Rust — без своего сервера.
</p>

<p align="center">
  <a href="https://github.com/newfpv/YaJarvis/releases/latest"><img src="https://img.shields.io/github/v/release/newfpv/YaJarvis?style=flat-square&amp;label=release&amp;color=f77b2c&amp;labelColor=202027" alt="Последний релиз"></a>
  <a href="https://github.com/newfpv/YaJarvis/actions/workflows/release.yml"><img src="https://img.shields.io/github/actions/workflow/status/newfpv/YaJarvis/release.yml?style=flat-square&amp;label=build&amp;labelColor=202027" alt="Сборка релиза"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-f77b2c?style=flat-square&amp;labelColor=202027" alt="Лицензия MIT"></a>
  <a href="#сколько-колонок-с-джарвисом"><img src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fnewfpv%2FYaJarvis%2Fstats-data%2Fbadge.json&amp;style=flat-square" alt="Колонок с Джарвисом"></a>
</p>

<p align="center">
  <a href="https://github.com/newfpv/YaJarvis/releases/latest"><strong>Скачать</strong></a> ·
  <a href="#установка"><strong>Установка</strong></a> ·
  <a href="docs/FAQ.md"><strong>FAQ и бесплатный старт</strong></a> ·
  <a href="docs/PLUGINS.md"><strong>Вики по плагинам</strong></a> ·
  <a href="#панель-управления"><strong>Панель</strong></a> ·
  <a href="https://github.com/newfpv/YaJarvis/issues"><strong>Обратная связь</strong></a>
</p>

![Главная панель YaJarvis: диалог, состояние колонки и музыкальный плеер](docs/screenshots/dashboard.png)

---

## Что умеет YaJarvis

YaJarvis работает на колонке рядом со штатной Алисой — на чистом Rust, без ПК или домашнего сервера 24/7.

<p align="center">
  <img src="docs/features.png" alt="Что умеет YaJarvis: локальное слово активации, 5 AI-провайдеров, YouTube и Алиса, умный дом, пулы ключей и VPN, веб-панель SvelteKit" width="100%">
</p>

---

## Проект бесплатный? Как пользоваться без оплаты

**Да, YaJarvis полностью бесплатен (лицензия MIT).** В проекте нет подписок автору, скрытых платежей и платного посредника — колонка общается напрямую с выбранным вами AI-сервисом по вашим ключам или через бесплатные/пользовательские плагины.

<p align="center">
  <a href="docs/FAQ.md#1-проект-бесплатный-нужна-ли-подписка-на-ии">
    <img src="docs/pricing.png" alt="Как пользоваться YaJarvis бесплатно: Google Gemini Live, ElevenLabs Free, OpenRouter :free и Ollama, работа без AI-ключей, ChatGPT Plus/Pro и OpenAI/Grok API" width="100%">
  </a>
</p>

<p align="center">
  <a href="docs/FAQ.md#google-gemini--бесплатно">Ключ Gemini</a> ·
  <a href="docs/FAQ.md#elevenlabs--живой-разговор-синтез-и-распознавание">Пул ключей ElevenLabs</a> ·
  <a href="docs/FAQ.md#как-пользоваться-нейросетями-полностью-бесплатно">OpenRouter :free и Ollama</a> ·
  <a href="docs/FAQ.md#можно-ли-пользоваться-совсем-без-ai-ключей">Без AI-ключей</a> ·
  <a href="docs/FAQ.md#chatgpt--нужен-только-plus-или-подойдёт-go-и-free">ChatGPT Plus vs Go/Free</a>
</p>

---

## Частые вопросы (коротко)

<details>
<summary><strong>Нужна ли какая-либо подписка на ИИ или можно пользоваться с бесплатной?</strong></summary>

Платная подписка **не обязательна**. Лучший бесплатный вариант — **Google Gemini Live** (бесплатные дневные лимиты без привязки карты + пул из ключей с разных аккаунтов Google). Также есть бесплатный тариф у **ElevenLabs** и бесплатные `:free` модели **OpenRouter** или локальная **Ollama** через Lua-плагин.

</details>

<details>
<summary><strong>Создал несколько ключей Gemini (или ElevenLabs), но лимит всё равно кончается — почему?</strong></summary>

Бесплатная квота у **Google Gemini** и **ElevenLabs** действует **на весь аккаунт целиком (у Gemini — на аккаунт / проект), а не на каждый отдельный API-ключ**. Если создать 5–10 ключей в одном аккаунте Google, все они будут делить один и тот же общий лимит.

Чтобы пул ключей реально увеличивал запас, добавляйте ключи с **разных аккаунтов**.

</details>

<details>
<summary><strong>Для ChatGPT нужен только Plus? Подойдёт ли ChatGPT Go или Free?</strong></summary>

Режим **«GPT-Live по подписке»** рассчитан на **ChatGPT Plus** и **Pro**. На **ChatGPT Free** и **ChatGPT Go** сам OpenAI обычно закрывает доступ к эндпоинтам Codex/бэкенда или жёстко ограничивает голосовой лимит — покупать **Go** специально ради Джарвиса не стоит. При исчерпании лимита подписки колонка умеет автоматически переключаться на ваш бесплатный ключ **Gemini**.

</details>

<details>
<summary><strong>Где достать ключи? Автор проекта помогает с ключами или выдаёт их?</strong></summary>

У каждого пользователя ключи личные — они хранятся только на колонке (`/data/opt/jarvisd/settings.json`), автор их не продаёт и не выдаёт. Никому не присылайте свои ключи в личные сообщения. Пошаговые инструкции по выпуску бесплатных ключей за 2 минуты — в **[FAQ](docs/FAQ.md#2-где-достать-ключи-и-как-подключить-сервисы)**.

</details>

<details>
<summary><strong>Как полностью удалить YaJarvis с колонки?</strong></summary>

1. **Через установщик `JarvisSetup`:** запустите его, выберите колонку и на экране проверки нажмите красную кнопку **«Удалить Джарвиса»**.
2. **Вручную через ADB:** выполните `adb connect IP-КОЛОНКИ:5555` и `adb -s IP-КОЛОНКИ:5555 shell "sh /data/opt/jarvisd/jarvisd.sh uninstall"`.

Колонка полностью удалит файлы Джарвиса и перезагрузится — системная прошивка и штатная Алиса останутся как были.

</details>

<details>
<summary><strong>Забыл пароль от веб-панели — как сбросить?</strong></summary>

На самой колонке нажмите поочерёдно кнопки **«громче», «тише» 8 раз подряд (`+ − + − + − + −`)** с интервалом не дольше 1,5 с. Громкость в итоге останется прежней, а пароль сбросится.

</details>

**[Открыть полный FAQ с пошаговыми инструкциями →](docs/FAQ.md)**

---

## Установка

Понадобится Яндекс Станция с **root-доступом и ADB по сети** и компьютер в той же локальной сети. Получение root-доступа описано в [Yandex Station Extended](https://hackalice.me/). Отдельный сервер или постоянно включённый компьютер после установки **не нужен**.

### Windows

Откройте **PowerShell** и выполните:

```powershell
irm https://github.com/newfpv/YaJarvis/releases/latest/download/install.ps1 | iex
```

### macOS и Linux

Откройте **Терминал** и выполните:

```sh
curl -fsSL https://github.com/newfpv/YaJarvis/releases/latest/download/install.sh | sh
```

Для macOS и Linux добавьте `adb` (Android Platform Tools) в `PATH`; на Windows установщик при необходимости сам подтянет `adb`. После запуска откроется страница установщика в браузере:

1. **Выберите колонку.** Установщик найдёт подключённые по ADB устройства или позволит ввести IP вручную, сам определит модель и архитектуру.
2. **Задайте пароль панели и нажмите «Установить».**
3. **Пройдите мастер в панели (`http://IP-КОЛОНКИ:8080`).** Подключите AI-сервис (например, бесплатный ключ Gemini или аккаунт ChatGPT), выберите нужные плагины и укажите свой город и часовой пояс во вкладке **«Колонка»**.

<details>
<summary><strong>Как выглядит установщик</strong></summary>

Во время установки справа показаны возможности и скриншоты интерфейса, а слева — прогресс и подробный журнал.

![Установщик YaJarvis со знакомством с возможностями проекта](docs/screenshots/installer.png)

</details>

<details>
<summary><strong>Скачать установщик вручную</strong></summary>

Вы можете просмотреть скрипт установки заранее или скачать готовый бинарник со страницы [последнего релиза](https://github.com/newfpv/YaJarvis/releases/latest):

| Система | Файл |
| :--- | :--- |
| Windows x64 | `JarvisSetup-windows-amd64.exe` |
| macOS Apple Silicon | `JarvisSetup-darwin-arm64` |
| macOS Intel | `JarvisSetup-darwin-amd64` |
| Linux x64 | `JarvisSetup-linux-amd64` |
| Linux ARM64 | `JarvisSetup-linux-arm64` |

На macOS и Linux выдайте права на запуск: `chmod +x JarvisSetup-*`. Бинарники не подписаны сертификатом разработчика. Для восстановления или удаления Джарвиса просто запустите установщик повторно.

</details>

---

## Плагины и расширения

В YaJarvis чёткое разделение между ядром и подключаемыми пакетами:

- **Встроенные в ядро (`jarvisd`).** Микрофон, локальный детектор слова «Джарвис», звук, кнопки, подсветка, связь с Алисой, память, веб-панель и **5 голосовых/AI-провайдеров** (`ChatGPT`, `OpenAI`, `Gemini`, `Grok`, `ElevenLabs`) — ничего лишнего запускать не нужно.
- **Официальные плагины (10 пакетов).** Устанавливаются прямо из мастера или в **«Подключения → Плагины»**:
  - **Lua 5.4 (лёгкие, в песочнице):** `weather` (Погода Open-Meteo без ключей), `currency` (Курсы ЦБ РФ / НБ РБ), `dice` (Монетка и кубик), `yandexhome` (Яндекс Умный дом).
  - **Rust (отдельные процессы):** `homeassistant`, `smartlife` (Tuya LAN + облако), `xiaomi` (Mi Home), `youtube` (музыка и аккаунт), `vpn` (VLESS / REALITY / Vision), `training` (смена фразы и дообучение слова активации).
- **Плагины сообщества и свои разработки.** Джарвис сам находит открытые плагины на GitHub по теме `yajarvis-plugin` (и категоризирует по `yajarvis-<категория>`), а также умеет ставить любой `.zip`-архив с ПК.

**[Открыть подробную Вики по плагинам →](docs/PLUGINS.md)** · **[Спецификация для AI-агентов (`PLUGIN-AGENT.md`) →](docs/PLUGIN-AGENT.md)** *(каталог, манифест `plugin.json`, Lua API, Rust SDK, шаблоны CI и публикация в каталог GitHub)*

---

## Панель управления

Скриншоты ниже показывают пример настроенной колонки. Чистая установка стартует пустой: **город не задан, часовой пояс UTC, личные аккаунты и ключи не подключены**.

<details open>
<summary><strong>Подключения — голосовые сервисы, умный дом, музыка и плагины</strong></summary>

![Подключения голосовых сервисов, умного дома и музыки](docs/screenshots/connections.png)

</details>

<details>
<summary><strong>Ассистенты — движок, голос, модель и характер</strong></summary>

![Выбор голосового движка и настройка ассистента](docs/screenshots/assistant.png)

</details>

<details>
<summary><strong>Графики — процессор, память, температура и Wi-Fi</strong></summary>

![Графики состояния колонки и нагрузки YaJarvis](docs/screenshots/charts.png)

</details>

---

## Колонки и автоопределение

Установщик сам считывает идентификаторы платы, процессор, ARM ABI штатной Алисы и проверяет доступное место на флешке. Выбирать файл под конкретную модель вручную не нужно.

| Семейство | Определяемые модели |
| :--- | :--- |
| **Лайт** | Станция Лайт, Станция Лайт 2 |
| **Мини** | Станция Мини, Мини 2, Мини 3, Мини 3 Про |
| **Миди** | Станция Миди / Cucumber |
| **Другие Станции** | Станция 2, Станция 3, Стрит |
| **Android-модели** | Станция Макс и Станция первого поколения определяются; установка пока не поддерживается |

Полный голосовой тракт **проверен на Станции Лайт первого поколения**. Для остальных моделей статус экспериментальный — работа микрофона и подсветки зависит от прошивки. Если на колонке нет нужных компонентов, установщик остановится с понятным объяснением. Яндекс Хаб не поддерживается.

<details>
<summary><strong>Почему Миди и Станция 2 определяются отдельно</strong></summary>

Внутреннее имя `station-midi` / `yandexmidi` в прошивках Яндекса относится к Станции 2, а розничная Станция Миди имеет кодовое имя `Cucumber`. Автоопределение учитывает это различие (подробности в [каталоге устройств](https://hackalice.me/devices/)).

</details>

---

## Обновления

В панели откройте **Безопасность → Обновления → Проверить → Обновить**.

Новая версия скачивается из [GitHub Releases](https://github.com/newfpv/YaJarvis/releases), подбирается под архитектуру колонки (`arm64` / `armv7`) и проверяется по **SHA-256** перед записью. При неудачном запуске `jarvis-supervisor` автоматически откатывает предыдущий рабочий бинарник. Автоматическая ночная установка (с 03:00 до 06:00 в простое) включается отдельно в настройках обновлений.

Для ручного обновления файлом загрузите в панель `jarvisd-arm64` или `jarvisd-armv7`.

---

## Сколько колонок с Джарвисом

<p align="center"><img src="https://raw.githubusercontent.com/newfpv/YaJarvis/stats-data/stats.svg" alt="Счётчик установок YaJarvis: колонки по моделям и версиям" width="100%"></p>

Счёт ведёт сам GitHub: при первом запуске и при смене версии колонка один раз скачивает пустой файл из служебного релиза [`stats`](https://github.com/newfpv/YaJarvis/releases/tag/stats). В имени файла передаются только модель колонки и версия YaJarvis — без IP-адресов, ключей и идентификаторов владельца. Статистика обновляется раз в 6 часов и отключается в панели: **Безопасность → Счётчик установок**.

---

## Документация

| Раздел | О чём внутри |
| :--- | :--- |
| **[FAQ и бесплатный старт](docs/FAQ.md)** | Бесплатные способы (Gemini Live, пулы ключей, ElevenLabs, OpenRouter `:free`), ChatGPT Plus vs Go/Free, получение ключей, VPN, удаление, сброс пароля |
| **[Вики по плагинам](docs/PLUGINS.md)** · **[Для AI-агентов](docs/PLUGIN-AGENT.md)** | Каталог 10 официальных плагинов, манифест `plugin.json`, категории, разрешения, Lua 5.4 API, Rust SDK, шаблоны GitHub Actions и публикация в каталог |
| **[Справочник Lua API](docs/LUA.md)** | Компактная шпаргалка по методам `jarvis.*`, `chat`, `transcribe`, `speak` и лимитам песочницы |
| **[Rust-пакеты и модули](docs/NATIVE-PLUGINS.md)** · **[Официальные пакеты](docs/RUST-MODULES.md)** | Создание бинарных плагинов на `jarvis-plugin-sdk` и встраивание модулей в ядро |
| **[Дообучение слова «Джарвис»](docs/WAKEWORD.md)** · **[Исследование активации](docs/WAKEWORD-RESEARCH.md)** | Сбор записей, разметка и дообучение моделей активации `OWWH`/`OWH2` и `JKWS1` под свой голос |
| **[Приватность и данные](docs/PRIVACY.md)** · **[Сборка из исходников](docs/BUILD.md)** | Как хранятся ключи, куда уходит трафик, как собрать прошивку локально |

---

## Поддержать проект и авторы

Если YaJarvis пригодился, поставьте **Star** вверху страницы. О багах и идеях пишите в [Issues](https://github.com/newfpv/YaJarvis/issues) — прикладывайте модель колонки, версию YaJarvis и шаги воспроизведения (предварительно убрав из логов личные токены и ключи).

### Contributors и авторы проектов

Участники и авторы открытых проектов, на которых держится и опирается YaJarvis:

<table>
<tr>
<td align="center" width="110"><a href="https://github.com/i7d3v3l0p3r"><img src="https://github.com/i7d3v3l0p3r.png?size=96" width="48" alt="i7d3v3l0p3r"><br><sub><b>i7d3v3l0p3r</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/dimap"><img src="https://github.com/dimap.png?size=96" width="48" alt="dimap"><br><sub><b>dimap</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/Marker284"><img src="https://github.com/Marker284.png?size=96" width="48" alt="Marker284"><br><sub><b>Marker284</b></sub></a></td>
</tr>
<tr>
<td align="center" width="110"><a href="https://github.com/rison1337"><img src="https://github.com/rison1337.png?size=96" width="48" alt="rison1337"><br><sub><b>rison1337</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/amirilivice"><img src="https://github.com/amirilivice.png?size=96" width="48" alt="amirilivice"><br><sub><b>amirilivice</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/tangenx"><img src="https://github.com/tangenx.png?size=96" width="48" alt="tangenx"><br><sub><b>tangenx</b></sub></a></td>
</tr>
<tr>
<td align="center" width="110"><a href="https://github.com/Morvex885"><img src="https://github.com/Morvex885.png?size=96" width="48" alt="Morvex885"><br><sub><b>Morvex885</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/nyakokitsu"><img src="https://github.com/nyakokitsu.png?size=96" width="48" alt="nyakokitsu"><br><sub><b>nyakokitsu</b></sub></a></td>
<td align="center" width="110"><a href="https://github.com/dscripka"><img src="https://github.com/dscripka.png?size=96" width="48" alt="dscripka"><br><sub><b>dscripka</b></sub></a></td>
</tr>
</table>

Спасибо **[i7d3v3l0p3r](https://github.com/i7d3v3l0p3r)** за перенос веб-панели и установщика на SvelteKit, доработки плагина «Обучение слова», установщика и профиля Firefox для VLESS REALITY.

**Дизайн YaJarvis вдохновлён [yapanel](https://github.com/dimap/yapanel-web) автора [dimap](https://github.com/dimap).** Тёмная тема, тёплый оранжевый акцент, карточки состояния и подача графиков стали визуальным ориентиром для нашей панели. Спасибо dimap за открытый проект.

Спасибо **[Marker284 и сообществу Yandex Station Extended](https://github.com/Marker284/yandex-station-extended)** за исследования Станций; **[rison1337](https://github.com/rison1337/Alisa-ChatGPT-voice-assistant)** и **[amirilivice](https://github.com/amirilivice/quasard)** — за проекты, послужившие отправной точкой работы с голосовым ассистентом на колонке.

Полный список авторов, использованных подходов и библиотек — в **[CREDITS.md](CREDITS.md)**. Уведомления об авторстве и лицензии сторонних компонентов сохранены в **[THIRD_PARTY.md](THIRD_PARTY.md)** и **[LICENSES](LICENSES)**.

---

[MIT License](LICENSE). Сторонние компоненты сохраняют свои лицензии.

YaJarvis — независимый проект сообщества, не официальный продукт производителей колонки и используемых AI-сервисов.
