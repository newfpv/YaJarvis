<script>
  import { icon } from "#helpers/core.js";
  import VpnSwitch from "./VpnSwitch.svelte";

  let { service, state, action, keydown } = $props();
  let id = $derived(service.id);
  let gemini = $derived(state.gemini_keys || []);
  let pool = $derived(state.gemini_pool || null);
  let eleven = $derived(state.eleven_keys || []);
  let append = $derived(service.on && ["gemini", "elevenlabs"].includes(id));
  const date = (seconds) => new Date(seconds * 1000).toLocaleDateString("ru-RU", { day: "numeric", month: "long" });
  const isBad = (status) => status === "invalid" || status === "quota_zero";
  const isRest = (status) => status === "minute" || status === "suspected" || status === "daily" || status === "partial";
</script>

{#if id === "gemini"}
  {#if gemini.length > 0}
    {#if pool && pool.models?.length > 0}
      <div class="gp-sum">
        <div class="gp-head">
          <b>Пул ключей: {pool.total_keys}</b>
          {#if pool.active_tail}<span class="gp-meta">активный #{(pool.active_index || 0) + 1} · {pool.active_tail}</span>{/if}
          <span class="spacer"></span>
          {#if pool.any_limited}
            <button type="button" class="btn sm ghost" data-gk-reset-all="1" data-tip="Сбросить паузы по лимитам для всех ключей" onclick={action}>Сбросить все лимиты</button>
          {/if}
        </div>
        <div class="ek-pills">
          {#each pool.models as m (m.id)}
            <span class={["ek-pill", m.free_keys === m.total_keys && m.total_keys > 0 ? "ok" : m.free_keys > 0 ? "warn" : "bad", { checked: m.checked }]}>
              {#if m.checked}✓ {/if}{m.name}: {m.label}
            </span>
          {/each}
        </div>
      </div>
    {/if}
    <div class="ek-list">
      {#each gemini as key, index}
        {@const bad = isBad(key.status)}
        {@const rest = isRest(key.status)}
        {@const active = Boolean(key.active) && !bad && !rest}
        {@const limitedModels = (key.model_states || []).filter((m) => m.status !== "ready")}
        <div class="ek-wrap">
          <div class="ek" class:on={active || (key.active && key.status === "partial")} class:rest={rest} class:bad={bad}>
            <b>{index + 1}</b><code>{key.tail}</code>
            <span class="ek-st">{key.label || (key.spent ? `лимит у ${key.spent} из ${key.models} моделей` : "готов")}</span>
            <span class="spacer"></span>
            {#if gemini.length > 1 && !key.active && !bad}
              <button type="button" class="btn sm ghost" data-gk-activate={index} data-tip="Использовать этот ключ первым" onclick={action}>Активный</button>
            {/if}
            {#if key.spent > 0 || bad || rest}
              <button type="button" class="btn sm ghost" data-gk-reset={index} data-tip="Сбросить паузу по лимиту и проверить ключ снова" onclick={action}>Сбросить</button>
            {/if}
            {#if gemini.length > 1}
              <button type="button" class="ib" data-gk-remove={index} aria-label={`Убрать ключ ${index + 1}`} onclick={action}>{@html icon("x")}</button>
            {/if}
          </div>
          {#if limitedModels.length > 0 || key.detail}
            <div class="ek-sub">
              {#if limitedModels.length > 0}
                <div class="ek-pills">
                  {#each key.model_states as model (model.id)}
                    <span class={["ek-pill", model.status === "ready" ? "ok" : isBad(model.status) ? "bad" : "warn"]}>
                      {model.name}: {model.label}
                    </span>
                  {/each}
                </div>
              {/if}
              {#if key.detail}<small>{key.detail}</small>{/if}
            </div>
          {/if}
        </div>
      {/each}
    </div>
    <p class="note">Бесплатный лимит Gemini общий на весь аккаунт Google, а не на каждый ключ отдельно: в запас добавляйте ключи с разных аккаунтов. Переключение между ключами работает автоматически; переключение на другую модель работает между моделями, отмеченными галочками в профиле ассистента.</p>
  {/if}
{:else if id === "elevenlabs" && eleven.length}
  <div class="ek-list">
    {#each eleven as key, index}
      <div class="ek" class:on={key.active} class:rest={key.until}>
        <b>{index + 1}</b><code>{key.tail}</code>
        <span class="ek-st">{key.until ? `квота кончилась · ждёт до ${date(key.until)}` : key.active ? "сейчас работает" : "в запасе"}</span>
        <span class="spacer"></span>
        {#if key.until}<button type="button" class="btn sm ghost" data-ek-reset={index} data-tip="Например, если докупили символы" onclick={action}>Вернуть</button>{/if}
        <button type="button" class="ib" data-ek-remove={index} aria-label={`Убрать ключ ${index + 1}`} onclick={action}>{@html icon("x")}</button>
      </div>
    {/each}
  </div>
  <p class="note">Бесплатная квота привязана к аккаунту — в запас добавляйте ключи с разных аккаунтов. Когда лимит кончится, ключ откладывается до сброса (обычно через месяц), и Джарвис сам берёт следующий.</p>
{:else if id === "yandex"}
  <div><b>Выдайте OAuth-приложению два разрешения</b><ul class="how">
    <li><code>iot:view</code> · видеть устройства и их состояние.</li>
    <li><code>iot:control</code> · управлять устройствами, группами и сценариями.</li>
  </ul></div>
{/if}

<div class="f">
  <label for={"k-" + id}>{id === "yandex" ? "OAuth-токен Яндекс ID" : append ? "Ещё ключ — в запас" : service.on ? "Новый ключ" : "Ключ"}</label>
  <div class="row">
    <input type="password" id={"k-" + id} placeholder={service.place} autocomplete="off" onkeydown={keydown}>
    <button type="button" class="btn primary" data-key={id} onclick={action}>{append ? "Добавить" : service.on ? "Заменить" : "Сохранить"}</button>
  </div>
</div>
{#if id !== "elevenlabs"}
  <details open={!service.on}>
    <summary>{id === "yandex" ? "Как получить токен" : "Где его взять"}</summary>
    <ol class="how">
      {#if id === "openai"}
        <li>Откройте platform.openai.com и войдите.</li>
        <li>Слева — API keys → Create new secret key.</li>
        <li>Скопируйте ключ (начинается с sk-) и вставьте сюда.</li>
        <li>Пополните баланс в разделе Billing — от 5 $.</li>
      {:else if id === "gemini"}
        <li>Откройте aistudio.google.com и войдите в аккаунт Google (из России и Беларуси — через VPN).</li>
        <li>Нажмите Get API key → Create API key.</li>
        <li>Скопируйте ключ и вставьте сюда.</li>
        <li>Для запасного ключа в пул создайте ключ в другом аккаунте Google: бесплатный лимит общий на весь аккаунт, а не на каждый ключ отдельно.</li>
      {:else if id === "grok"}
        <li>Откройте console.x.ai и войдите.</li>
        <li>Пополните баланс (Billing → Credits) — без кредитов ключ не работает.</li>
        <li>API Keys → Create API key, скопируйте ключ (начинается с xai-) и вставьте сюда.</li>
      {:else}
        <li>В <a href="https://oauth.yandex.ru/client/new/" target="_blank" rel="noopener">Яндекс OAuth</a> создайте приложение «Для доступа к API или отладки». В разделе «Доступы» добавьте оба разрешения.</li>
        <li>Укажите Redirect URI <code>https://oauth.yandex.ru/verification_code</code> и сохраните приложение.</li>
        <li>Войдите в тот Яндекс ID, где добавлены устройства, и <a href="https://yandex.ru/dev/id/doc/ru/tokens/debug-token" target="_blank" rel="noopener">получите токен вручную</a>. Вставьте сюда значение <code>access_token</code>.</li>
      {/if}
    </ol>
  </details>
{/if}
<VpnSwitch {id} {state} {action} />
