import { ClipboardCopyIcon, PlayIcon } from '@radix-ui/react-icons'
import { useEffect, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { createRoot } from 'react-dom/client'
import { Player } from './Player'
import { createRoom, errorText, useRoom } from './room'
import type { Invitation } from './room'
import './styles.css'

function VideoLink({ id, value, onChange, disabled }: { id: string; value: string; onChange: (value: string) => void; disabled: boolean }) {
  return (
    <input id={id} type="url" autoComplete="off" autoCapitalize="off" spellCheck={false} inputMode="url"
      placeholder="https://…" value={value} onChange={(event) => onChange(event.target.value)} required disabled={disabled} />
  )
}

function CreateRoom() {
  const [url, setUrl] = useState('')
  const [pending, setPending] = useState(false)
  const [error, setError] = useState('')

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (pending) return
    setPending(true)
    setError('')
    try {
      const roomId = await createRoom(url.trim())
      location.assign(`/r/${encodeURIComponent(roomId)}`)
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : errorText('connection'))
      setPending(false)
    }
  }

  return (
    <main className="create-layout" id="main">
      <div>
        <h1>Смотреть вместе</h1>
        <p className="subtitle">Создайте комнату и отправьте приглашение. Пауза и перемотка будут общими.</p>
      </div>
      <form className="create-form" onSubmit={submit} aria-busy={pending}>
        <label htmlFor="source">Ссылка на видео</label>
        <div className="field-row">
          <VideoLink id="source" value={url} onChange={setUrl} disabled={pending} />
          <button className="button primary" type="submit" disabled={pending}>{pending ? 'Создаём…' : 'Создать комнату'}</button>
        </div>
        <p className="help">YouTube, прямая ссылка MP4, HLS или DASH, либо HTTPS-страница с открытым видео. Только записи.</p>
        {error && <p className="form-error" role="alert">{error}</p>}
      </form>
    </main>
  )
}

function Invite({ invitation, connected, setControl }: {
  invitation: Invitation
  connected: boolean
  setControl: (canControl: boolean) => void
}) {
  const { number, token, canControl } = invitation
  const link = new URL(`/i/${token}`, location.origin).href
  const nameId = `guest-${number}`
  const [result, setResult] = useState<'copied' | 'manual'>()
  const input = useRef<HTMLInputElement>(null)

  function selectLink() {
    input.current?.focus()
    input.current?.select()
  }

  useEffect(() => {
    if (result === 'manual') selectLink()
  }, [result])

  async function copy() {
    try {
      await navigator.clipboard.writeText(link)
      setResult('copied')
    } catch {
      setResult('manual')
      selectLink()
    }
  }

  return (
    <li className="invite-row">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <span id={nameId} className="font-semibold">Гость {number}</span>
        <label className="inline-flex min-h-11 items-center gap-2 text-sm">
          <input type="checkbox" className="size-4 shrink-0 accent-accent" aria-describedby={nameId}
            checked={canControl} disabled={!connected} onChange={(event) => setControl(event.target.checked)} />
          Может управлять видео
        </label>
        <button type="button" className="button" onClick={copy} aria-describedby={nameId}>
          <ClipboardCopyIcon aria-hidden="true" />Скопировать ссылку
        </button>
        {result === 'copied' && <span className="help" role="status">Ссылка скопирована</span>}
      </div>
      {result === 'manual' && <label className="mt-2 grid gap-1.5 text-sm">Скопируйте ссылку вручную
        <input ref={input} readOnly value={link} onFocus={(event) => event.currentTarget.select()} />
      </label>}
    </li>
  )
}

const connection = {
  connecting: 'Подключаемся…', connected: 'На связи', reconnecting: 'Переподключаемся…', offline: 'Нет сети', closed: 'Нет доступа',
}
const sourceNames = { youtube: 'YouTube', mp4: 'Прямой MP4', hls: 'HLS-запись', dash: 'DASH-запись' }

function Room({ roomId, inviteToken }: { roomId: string; inviteToken: string | null }) {
  const { status, state, role, canControl, guestNumber, invitations, serverNow, error, send } = useRoom(roomId, inviteToken)
  const [url, setUrl] = useState('')
  const connected = status === 'connected'
  const host = role === 'host'

  function load(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (host && connected) send({ type: 'load', url: url.trim() })
  }

  return (
    <main className="room-layout" id="main">
      <div className="room-heading">
        <div className="mb-3 flex flex-wrap items-center gap-2">
          {role && status !== 'closed' && <span className="pill">{host ? 'Ведущий' : `Гость ${guestNumber}`}</span>}
          <span className="connection" role="status" data-connected={connected}><i aria-hidden="true" />{connection[status]}</span>
        </div>
        <h1>Комната просмотра</h1>
        {state && status !== 'closed' && <p className="source-label">{sourceNames[state.source.type]}</p>}
      </div>
      {status === 'closed' ? (
        <div className="room-unavailable">
          <p className="form-error" role="alert">{errorText(error ?? 'room_not_found')}</p>
          <a className="button" href="/">Создать новую комнату</a>
        </div>
      ) : state ? (
        <>
          <Player key={state.source.type === 'youtube' ? `youtube:${state.source.videoId}` : `${state.source.type}:${state.source.url}`}
            {...{ state, serverNow, canControl, connected, send }} />
          {error && <p className="form-error" role="alert">{errorText(error)}</p>}
          {host && <>
            <form className="source-form" onSubmit={load}>
              <label htmlFor="new-source">Новое видео</label>
              <div className="field-row">
                <VideoLink id="new-source" value={url} onChange={setUrl} disabled={!connected} />
                <button className="button" type="submit" disabled={!connected}>Загрузить</button>
              </div>
            </form>
            <section className="invite" aria-labelledby="invite-heading">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <h2 id="invite-heading">Приглашения</h2>
                <button className="button" type="button" disabled={!connected} onClick={() => send({ type: 'create_invitation' })}>Создать приглашение</button>
              </div>
              {invitations?.length ? (
                <ul className="divide-y divide-line" role="list">
                  {invitations.map((invitation) => <Invite key={invitation.number} {...{ invitation, connected }}
                    setControl={(canControl) => send({ type: 'set_guest_control', guestNumber: invitation.number, canControl })} />)}
                </ul>
              ) : <p className="help">Создайте отдельную ссылку для каждого гостя.</p>}
            </section>
          </>}
        </>
      ) : (
        <div className="screen" aria-busy="true">
          <div className="screen-message"><div className="skeleton" />{status === 'offline' ? 'Ждём подключения к сети' : 'Ждём данные комнаты…'}</div>
        </div>
      )}
    </main>
  )
}

function App() {
  const { pathname, search } = location
  const roomId = /^\/r\/([\w-]+)\/?$/.exec(pathname)?.[1]
  return (
    <div className="shell">
      <a className="sr-only focus:not-sr-only" href="#main">К содержимому</a>
      <header className="site-header">
        <a className="brand" href="/" aria-label="Share Video — главная">
          <span className="brand-mark"><PlayIcon aria-hidden="true" /></span><span>Share Video</span>
        </a>
      </header>
      {roomId ? <Room roomId={roomId} inviteToken={new URLSearchParams(search).get('invite')} /> : pathname === '/' ? <CreateRoom /> : (
        <main className="room-layout" id="main">
          <h1>Страница не найдена</h1>
          <p className="subtitle">Проверьте, полностью ли скопировано приглашение.</p>
          <a className="button" href="/">На главную</a>
        </main>
      )}
    </div>
  )
}

createRoot(document.getElementById('root')!).render(<App />)
