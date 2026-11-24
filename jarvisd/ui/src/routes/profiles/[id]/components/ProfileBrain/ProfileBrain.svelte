<script>
  import { untrack } from "svelte";
  import { tip } from "#helpers/core.js";
  import { S } from "#state/panelState.js";
  import { usesOpenAiModel } from "../profileDraft.js";
  import { brainOptions, modelKind } from "./options.js";

  const effortLabels = { none: "не думать", low: "немного", medium: "средне", high: "долго" };
  const llmPlaceholders = { gemini: "gemini-3.8-flash", grok: "grok-4-1-fast-non-reasoning" };
  let { initial, profile, engines, ttsProvider, engineReady, onedit, onchange, onrefreshvoices } = $props();
  const original = untrack(() => initial);
  const { providers, models, initial: defaults } = brainOptions(S, original);
  let draft = $state(defaults);
  let idle = $state(original.idle_s);
  let effort = $state(availableEffort(defaults.backend, original.effort));
  let classic = $derived(profile.engine === "classic");
  let openAiModel = $derived(profile.engine === "openai" || profile.engine === "chatgpt" || (classic && usesOpenAiModel(profile.llm_engine)));
  let sttKind = $derived(modelKind(profile.stt_engine));
  let ttsKind = $derived(modelKind(profile.tts_engine));
  let efforts = $derived(effortsFor(draft.backend));
  let geminiPoolById = $derived(
    Object.fromEntries((S.gemini_pool?.models || []).map((m) => [m.id, m]))
  );

  function effortsFor(model) {
    return S.model_efforts?.[model] || S.efforts || [];
  }

  function availableEffort(model, current) {
    const allowed = effortsFor(model);
    if (allowed.includes(current)) return current;
    return allowed.includes("low") ? "low" : allowed[0] || "low";
  }

  function changeBackend(value) {
    draft.backend = value;
    effort = availableEffort(value, effort);
    onchange();
  }

  function toggleGeminiModel(id, checked) {
    const order = models.geminiInfo.map((m) => m.id);
    const current = new Set(draft.geminiModels || []);
    if (checked) {
      current.add(id);
    } else if (current.size > 1) {
      current.delete(id);
    } else {
      return;
    }
    const next = order.filter((m) => current.has(m));
    for (const m of current) {
      if (!next.includes(m)) next.push(m);
    }
    draft.geminiModels = next;
    draft.gemini = next[0] || draft.gemini;
    onchange();
  }

  export function getValues() {
    const geminiModels = draft.geminiModels?.length ? [...draft.geminiModels] : (draft.gemini ? [draft.gemini] : []);
    return {
      backend: draft.backend, stt: draft.stt[sttKind].trim(), tts: draft.tts[ttsKind].trim(), customLlm: draft.customLlm.trim(),
      live: profile.engine === "gemini" ? (geminiModels[0] || "") : profile.engine === "eleven_realtime" ? draft.realtime : "",
      geminiModels,
      idle: Number(idle), effort,
    };
  }

  export function updateIdle(value) {
    idle = value;
  }
</script>

{#snippet options(items)}
  {#each items as [id, label] (id)}
    <option value={id}>{label}</option>
  {/each}
{/snippet}

{#snippet stageModel(stage, label, kind, items)}
  {@const id = `p-${stage}-model-${kind === "elevenlabs" ? "el" : kind === "openai" ? "oa" : "custom"}`}
  <div class="f">
    <label for={id}>{label}</label>
    {#if kind === "custom"}
      <input {id} type="text" maxlength="160" bind:value={draft[stage].custom} oninput={onchange}
        placeholder="По умолчанию плагина" autocomplete="off">
    {:else}
      <select {id} bind:value={draft[stage][kind]} onchange={onchange}>
        {@render options(items)}
      </select>
    {/if}
  </div>
{/snippet}

<section class="sec">
  <div class="sec-h">
    <h2>Мозг</h2>
    <p>Выберите живой разговор или свой пайплайн: распознавание речи → текстовая модель → озвучка</p>
  </div>
  <div class="sec-b">
    {#if engines.length}
      <div class="choice" id="p-engine" role="group" aria-label="Голосовой режим">
        {#each engines as { id, name, description } (id)}
          <button class="opt" class:on={profile.engine === id} data-engine={id} type="button"
            aria-pressed={profile.engine === id} onclick={() => profile.engine !== id && onedit({ engine: id })}>
            <b>{name}</b><small>{description}</small>
          </button>
        {/each}
      </div>
    {:else}
      <p class="note" id="p-no-engine">Установите и включите голосовой плагин в <a href="/connections/plugins">«Подключениях»</a>.</p>
    {/if}

    {#if engineReady && classic}
      <div class="eng stack">
        <div class="grid g2">
          <div class="f">
            <label for="p-stt-engine">1. Распознавание речи (STT)</label>
            <select id="p-stt-engine" value={profile.stt_engine} onchange={(e) => onedit({ stt_engine: e.currentTarget.value })}>
              {@render options(providers.stt)}
            </select>
          </div>
          {@render stageModel("stt", "Модель STT", sttKind, models.stt[sttKind])}
        </div>
        <div class="grid g2">
          <div class="f">
            <label for="p-llm-engine">2. Текстовая модель (LLM)</label>
            <select id="p-llm-engine" value={profile.llm_engine} onchange={(e) => onedit({ llm_engine: e.currentTarget.value })}>
              {@render options(providers.llm)}
            </select>
          </div>
          {#if !usesOpenAiModel(profile.llm_engine)}
            <div class="f" id="p-llm-custom-box">
              <label for="p-llm-model-custom">Модель (необязательно)</label>
              <input id="p-llm-model-custom" type="text" maxlength="160" bind:value={draft.customLlm} oninput={onchange}
                placeholder={llmPlaceholders[profile.llm_engine] || "deepseek/deepseek-chat-v3-0324"} autocomplete="off">
            </div>
          {/if}
        </div>
      </div>
    {:else if engineReady && profile.engine === "eleven_realtime"}
      <div class="eng"><div class="f half">
        <label for="p-eleven-rtmodel">Разговорная модель</label>
        <select id="p-eleven-rtmodel" bind:value={draft.realtime} onchange={onchange}>
          {@render options(models.realtime)}
        </select>
      </div></div>
    {:else if engineReady && profile.engine === "gemini"}
      <div class="eng"><div class="f">
        <span class="lbl">Модели Gemini {@html tip("Отметьте одну или несколько моделей. Джарвис начнёт с первой отмеченной; если на ней кончится лимит на всех ключах, сам перейдёт на следующую отмеченную. Неотмеченные модели не используются.")}</span>
        <div class="gm-list" id="p-gmodels" role="group" aria-label="Модели Gemini">
          {#each models.geminiInfo as item (item.id)}
            {@const idx = (draft.geminiModels || []).indexOf(item.id)}
            {@const checked = idx >= 0}
            {@const sole = checked && (draft.geminiModels || []).length <= 1}
            {@const poolInfo = geminiPoolById[item.id]}
            {@const stCls = !poolInfo || !poolInfo.total_keys ? "" : poolInfo.free_keys === poolInfo.total_keys ? "ok" : poolInfo.free_keys > 0 ? "warn" : "bad"}
            <label class={["sw hu gm-item", { on: checked }]}>
              <input type="checkbox" {checked} disabled={sole}
                onchange={(event) => {
                  const input = event.currentTarget;
                  const next = input.checked;
                  input.checked = checked;
                  toggleGeminiModel(item.id, next);
                }}>
              <span class="tr"></span>
              <span class="hu-t">
                <b>
                  {item.name}
                  {#if idx === 0}<span class="gm-tag primary">основная</span>
                  {:else if idx > 0}<span class="gm-tag">резерв #{idx}</span>{/if}
                  {#if poolInfo && poolInfo.total_keys > 0}
                    <span class={["gm-st", stCls]}>{poolInfo.label}</span>
                  {/if}
                </b>
                {#if item.note}<small>{item.note}</small>{/if}
              </span>
            </label>
          {/each}
        </div>
      </div></div>
    {/if}

    {#if engineReady && openAiModel}
      <div id="p-openai-model-box">
        <div class="f half">
          <label for="p-model">Текстовая модель {@html tip("Чем умнее модель, тем дольше она думает. Для голосового помощника обычно хватает самой быстрой.")}</label>
          <select id="p-model" value={draft.backend} onchange={(e) => changeBackend(e.currentTarget.value)}>
            {@render options(models.backend)}
          </select>
        </div>
        <div class="f">
          <span class="lbl">Сколько думать {@html tip("Доступные уровни зависят от выбранной текстовой модели: не все модели поддерживают «не думать». Немного — быстрее, средне и долго — для сложных вопросов.")}</span>
          <div class="seg" id="p-effort" role="group" aria-label="Сколько думать">
            {#each efforts as option (option)}
              <button type="button" data-v={option} class:on={option === effort} aria-pressed={option === effort}
                onclick={() => { if (effort !== option) { effort = option; onchange(); } }}>{effortLabels[option] || option}</button>
            {/each}
          </div>
        </div>
        <label class="sw">
          <input type="checkbox" id="p-priority" checked={profile.priority} onchange={(e) => onedit({ priority: e.currentTarget.checked })}>
          <span class="tr"></span>Приоритет {@html tip("Запросы обрабатываются в быстрой очереди OpenAI: ответ быстрее, но стоит дороже.")}
        </label>
      </div>
    {/if}
    {#if engineReady && !classic && providers.text.length}
      <div class="f">
        <label for="p-llm-plugin">Текстовая модель из плагина</label>
        <select id="p-llm-plugin" value={profile.llm_plugin || ""} onchange={(e) => onedit({ llm_plugin: e.currentTarget.value })}>
          <option value="">По настройкам ассистента</option>
          {@render options(providers.text)}
        </select>
        <small>Используется в переписке и при ответе голосовому плагину.</small>
      </div>
    {/if}
    {#if engineReady && classic}
      <div class="eng stack"><div class="grid g2">
        <div class="f">
          <label for="p-tts-engine">3. Озвучка (TTS)</label>
          <select id="p-tts-engine" value={profile.tts_engine} onchange={(e) => onedit({ tts_engine: e.currentTarget.value })}>
            {@render options(providers.tts)}
          </select>
        </div>
        {@render stageModel("tts", "Модель озвучки", ttsKind, models.tts[ttsKind])}
      </div></div>
    {/if}
    {#if engineReady && ttsProvider === "elevenlabs"}
      <div id="p-el-voices-bar">
        <p class="note">Голос выбирается ниже из вашего аккаунта ElevenLabs.</p>
        <button class="btn" id="p-el-refresh" type="button" onclick={onrefreshvoices}>Обновить голоса ElevenLabs</button>
      </div>
    {/if}
    <div class="stack">
      <label class="sw">
        <input type="checkbox" id="p-web" checked={profile.web_search} onchange={(e) => onedit({ web_search: e.currentTarget.checked })}>
        <span class="tr"></span>Поиск в интернете {@html tip("Для погоды, новостей, курсов и всего свежего. Без него Джарвис знает только то, чему его научили.")}
      </label>
    </div>
    <div class="f narrow">
      <label for="p-idle">Конец разговора, с {@html tip("Если вы оба молчите столько секунд, разговор закрывается и колонка снова ждёт «Джарвис». Пока разговор открыт, звать по имени не нужно.")}</label>
      <input type="number" id="p-idle" min="2" max="60" step="1" bind:value={idle} oninput={onchange}>
    </div>
  </div>
</section>
