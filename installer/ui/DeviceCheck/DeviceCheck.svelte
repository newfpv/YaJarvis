<script>
  import styles from "./DeviceCheck.module.css";

  let { api, upload, selectedSpeaker, onBack, onContinue, onRestored, onUninstalled } = $props();
  let speaker = $derived($selectedSpeaker);
  let adbAddress = $derived(speaker?.adb_address ?? speaker?.ip);
  let problems = $derived(speaker?.problems ?? []);
  let uninstalling = $state(false);
  let restoring = $state(false);
  let restoreError = $state("");

  function checksFor(speaker) {
    const autostartAndAliceOk = !problems.some((problem) =>
      /launch\.json|init\.d|start-stop-daemon|aplay|профил/.test(problem),
    );
    return [
      [speaker.root, "Root-доступ по ADB", speaker.root ? "есть" : "нужна разлоченная колонка"],
      [
        !speaker.android,
        "Прошивка на Linux",
        speaker.android ? "Android-Станции пока не поддерживаются" : "подходит",
      ],
      [
        Boolean(speaker.userland),
        "Микрофон Алисы",
        speaker.userland ? `библиотека ${speaker.userland}` : "не удалось определить",
      ],
      [
        autostartAndAliceOk,
        "Автозапуск и Алиса",
        autostartAndAliceOk ? "найдены" : "не подтверждены",
      ],
    ];
  }

  function technicalDetails(speaker, adbAddress) {
    return [
      ["Адрес ADB", adbAddress],
      ["Модель", speaker.model || "?"],
      ["Процессор", speaker.arch || "?"],
      ["Алиса / микрофон", speaker.userland || "?"],
      ["Root", speaker.root ? "есть" : "нет"],
      ["Память", speaker.ram_mb ? `${speaker.ram_mb} МБ` : "?"],
      ["Свободно места", speaker.free_mb ? `${speaker.free_mb} МБ` : "?"],
      ["Джарвис", speaker.installed || "не установлен"],
    ];
  }

  function spacePercent(available, needed) {
    if (available == null || needed == null) return null;
    return Math.max(3, available / Math.max(needed * 2, available, 1) * 100);
  }

  async function uninstall() {
    if (uninstalling || restoring || !confirm("Удалить Джарвиса с колонки? Она перезагрузится, Алиса останется как была.")) return;

    uninstalling = true;
    try {
      await api("POST", "/api/uninstall", { ip: adbAddress });
      onUninstalled();
    } catch (error) {
      alert(error.message);
    } finally {
      uninstalling = false;
    }
  }

  async function pickBackup(event) {
    const input = event.currentTarget;
    const file = input.files?.[0];
    input.value = "";
    if (!file || restoring || uninstalling || !adbAddress) return;
    restoreError = "";
    restoring = true;
    try {
      const info = await upload("/api/backup/inspect", file);
      if (!confirm(`Восстановить бэкап (версия ${info.app_version || "?"}, файлов: ${info.files}) на эту колонку? Текущие настройки будут заменены.`)) {
        return;
      }
      await api("POST", "/api/restore", { ip: adbAddress, backup_id: info.id });
      onRestored();
    } catch (error) {
      restoreError = error.message;
    } finally {
      restoring = false;
    }
  }
</script>

{#snippet tile(label, value, caption, unit, percent, lowSpace)}
  <div class={[styles.tile, lowSpace && styles.lowSpace]}>
    <span class={styles.tileLabel}>{label}</span>
    <span class={styles.tileValue}>{value}{#if unit}<small>{unit}</small>{/if}</span>
    {#if percent != null}
      <span class={styles.meter} aria-hidden="true"><i style:width={percent + "%"}></i></span>
    {/if}
    <small class={styles.tileCaption}>{caption}</small>
  </div>
{/snippet}

<div class="step-body">
  {#if speaker}
    {@const available = speaker.staging_mb ?? speaker.free_mb}
    {@const needed = speaker.need_mb}
    <span class="eyebrow">
      {adbAddress} · {speaker.installed ? `Джарвис ${speaker.installed} — обновим, настройки сохранятся` : "проверка устройства"}
    </span>
    <h1 class={styles.heading}>{speaker.model || "Колонка"}</h1>
    <div class={styles.tiles}>
      {@render tile(
        "Место для установки",
        available ?? "?",
        needed == null ? "Уточним после загрузки пакета" : `Нужно до ${needed} МиБ`,
        "МиБ",
        spacePercent(available, needed),
        speaker.space_ok === false,
      )}
      {@render tile(
        "Память",
        speaker.ram_mb || "?",
        speaker.ram_mb && speaker.ram_mb < 200 ? "впритык, но работать будет" : "хватает",
        "МБ",
      )}
      {@render tile(
        "Джарвис",
        speaker.installed || "—",
        speaker.installed ? "будет обновлён" : "ещё не установлен",
      )}
    </div>
    <ul class={styles.checks} aria-label="Проверка колонки">
      {#each checksFor(speaker) as [passed, label, description], index}
        <li class={passed ? undefined : styles.failed} style:--i={index} title={description}>
          <i aria-hidden="true"></i>
          <span>
            {label}
            {#if !passed}<small>{description}</small>{/if}
          </span>
        </li>
      {/each}
    </ul>
    <div>
      {#each problems as problem}
        <div class={[styles.message, styles.problem]}>{problem}</div>
      {/each}
      {#each speaker.notes ?? [] as note}
        <div class={styles.message}>{note}</div>
      {/each}
    </div>
    <details class={styles.technicalDetails}>
      <summary>Технические сведения</summary>
      <dl class={styles.technicalValues}>
        {#each technicalDetails(speaker, adbAddress) as [label, value]}
          <dt>{label}</dt>
          <dd>{value}</dd>
        {/each}
      </dl>
    </details>
    {#if speaker.installed}
      <div class={styles.extraActions}>
        <label class={[styles.restoreLink, restoring && styles.disabled]}>
          {restoring ? "Проверяю бэкап…" : "Восстановить из бэкапа…"}
          <input type="file" accept=".zip,application/zip" disabled={uninstalling || restoring} onchange={pickBackup}>
        </label>
        <button class="btn danger" type="button" onclick={uninstall} disabled={uninstalling || restoring}>Удалить Джарвиса</button>
      </div>
      {#if restoreError}
        <p class={styles.restoreError} role="alert">{restoreError}</p>
      {/if}
    {/if}
  {/if}
</div>

{#if speaker}
  <div class="step-footer">
    <div class="actions">
      <button class="btn ghost" type="button" onclick={onBack} disabled={uninstalling || restoring}>Назад</button>
      <button
        class="btn accent"
        type="button"
        onclick={() => onContinue(speaker)}
        disabled={uninstalling || restoring || problems.length > 0 || speaker.space_ok === false}
      >
        {speaker.installed ? "Обновить" : "Дальше"}
      </button>
    </div>
  </div>
{/if}
