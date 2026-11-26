import { PlayIcon } from '@radix-ui/react-icons'
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { FormEvent } from 'react'
import { mountVideo, mountYouTube } from './media'
import type { MediaPlayer } from './media'
import type { Command, RoomState } from './room'
import { needsSeek, positionAt } from './timing'

type Mode = 'loading' | 'ready' | 'enabled' | 'blocked' | 'failed'
type Props = {
  state: RoomState
  serverNow: () => number
  canControl: boolean
  connected: boolean
  send: (command: Command) => boolean
}

function clock(sec: number) {
  const seconds = Math.floor(sec)
  const h = Math.floor(seconds / 3600)
  const m = Math.floor(seconds / 60) % 60
  const s = String(seconds % 60).padStart(2, '0')
  return h ? `${h}:${String(m).padStart(2, '0')}:${s}` : `${m}:${s}`
}

function RoomPlayback({ state, serverNow, duration, connected, play, send }: Omit<Props, 'canControl'> & {
  duration: number
  play: () => void
}) {
  const [position, setPosition] = useState(0)
  const [seek, setSeek] = useState('')
  useEffect(() => {
    const tick = () => setPosition(Math.floor(Math.min(positionAt(state, serverNow()), duration || Infinity)))
    tick()
    if (state.paused) return
    const timer = window.setInterval(tick, 1000)
    return () => window.clearInterval(timer)
  }, [state, serverNow, duration])

  function submitSeek(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (seek === '') return
    const positionSec = Number(seek)
    if (!Number.isFinite(positionSec) || positionSec < 0) return
    if (send({ type: 'seek', positionSec })) setSeek('')
  }

  return (
    <section className="room-controls" aria-labelledby="controls-heading">
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <h2 id="controls-heading">Управление комнатой</h2>
        <p className="room-clock" aria-label="Позиция комнаты">{clock(position)}{duration > 0 ? ` / ${clock(duration)}` : ''}</p>
      </div>
      <div className="control-row">
        <button type="button" className="button primary" disabled={!connected} onClick={state.paused ? play : () => send({ type: 'pause' })}>
          {state.paused ? 'Воспроизвести' : 'Пауза'}
        </button>
        <form className="seek-form" onSubmit={submitSeek}>
          <label className="sr-only" htmlFor="seek">Позиция в секундах</label>
          <input id="seek" type="number" min="0" max={duration > 0 ? Math.floor(duration) : undefined} step="1" inputMode="numeric"
            placeholder="Секунды" value={seek} onChange={(event) => setSeek(event.target.value)} required disabled={!connected} />
          <button type="submit" className="button" disabled={!connected || seek === ''}>Перемотать</button>
        </form>
      </div>
    </section>
  )
}

export function Player({ state, serverNow, canControl, connected, send }: Props) {
  const type = state.source.type
  const uri = type === 'youtube' ? state.source.videoId : state.source.url
  const video = useRef<HTMLVideoElement>(null)
  const youtube = useRef<HTMLDivElement>(null)
  const player = useRef<MediaPlayer | null>(null)
  const snapshot = useRef(state)
  const controls = useRef({ canControl, connected, send })
  const pending = useRef<{
    revision: number
    expiresAt: number
    change: { paused: boolean } | { positionSec: number }
  } | null>(null)
  const modeRef = useRef<Mode>('loading')
  const [mode, setMode] = useState<Mode>('loading')
  const [failure, setFailure] = useState('')
  const [duration, setDuration] = useState(0)
  const [attempt, setAttempt] = useState(0)

  const sync = useCallback((immediate: boolean, gesture = false) => {
    const media = player.current
    if (!media || modeRef.current !== 'enabled') return
    const next = snapshot.current
    const local = pending.current
    if (local) {
      const applied = next.revision > local.revision && ('paused' in local.change
        ? next.paused === local.change.paused : next.positionSec === local.change.positionSec)
      // Don't correct a local action back to the old room state before the server echoes it.
      if (controls.current.connected && performance.now() < local.expiresAt && !applied) return
      pending.current = null
    }
    const length = media.duration()
    const limit = Number.isFinite(length) && length > 0 ? length : Infinity
    const target = positionAt(next, serverNow())
    const position = Math.min(target, limit)
    media.sync({
      position,
      playing: !next.paused && target < limit,
      seek: !needsSeek(media.position(), position, immediate) ? 'none' : immediate ? 'immediate' : 'drift',
      gesture,
    })
  }, [serverNow])

  useLayoutEffect(() => {
    const revoked = controls.current.canControl && !canControl
    const disconnectedWithPending = !connected && pending.current !== null
    const stateChanged = snapshot.current !== state
    controls.current = { canControl, connected, send }
    snapshot.current = state
    if (!connected || !canControl) pending.current = null
    if (stateChanged || revoked || disconnectedWithPending) sync(true)
  }, [state, canControl, connected, send, sync])

  useEffect(() => {
    let close: (() => void) | undefined
    let firstFrame = true
    function stop() {
      window.clearTimeout(loading)
      window.clearInterval(timer)
      const dispose = close
      close = undefined
      player.current = null
      dispose?.()
    }
    function fail(message: string) {
      updateMode('failed')
      setFailure(message)
      stop()
    }
    const loading = window.setTimeout(() => fail('Плеер не загрузился. Проверьте доступ к источнику и попробуйте ещё раз.'), 25_000)
    const timer = window.setInterval(() => sync(false), 1800)
    const events = {
      ready(media: MediaPlayer) {
        window.clearTimeout(loading)
        player.current = media
        updateDuration()
        updateMode('ready')
      },
      started() {
        if (!player.current) return
        const immediate = firstFrame || modeRef.current === 'blocked'
        firstFrame = false
        updateDuration()
        updateMode('enabled')
        sync(immediate)
      },
      canSeek: () => controls.current.canControl && controls.current.connected,
      seeked(positionSec: number) {
        const { canControl, connected, send } = controls.current
        if (!canControl || !connected || !Number.isFinite(positionSec) || positionSec < 0) return false
        if (!send({ type: 'seek', positionSec })) return false
        pending.current = { change: { positionSec }, revision: snapshot.current.revision, expiresAt: performance.now() + 5000 }
        return true
      },
      playbackChanged(paused: boolean) {
        const { canControl, connected, send } = controls.current
        if (!canControl || !connected || !player.current) return
        const current = snapshot.current
        const change = pending.current?.change
        if (paused === (change && 'paused' in change ? change.paused : current.paused)) return
        if (send({ type: paused ? 'pause' : 'play' })) {
          pending.current = { change: { paused }, revision: current.revision, expiresAt: performance.now() + 5000 }
        }
      },
      blocked() {
        updateMode('blocked')
      },
      failed: fail,
    }
    close = type === 'youtube' ? mountYouTube(youtube.current!, uri, events) : mountVideo(video.current!, type, uri, events)
    if (modeRef.current === 'failed') stop()
    return stop
  }, [attempt, type, uri, sync])

  function updateMode(next: Mode) {
    modeRef.current = next
    setMode(next)
  }

  function updateDuration() {
    const length = player.current?.duration() ?? 0
    if (length > 0 && Number.isFinite(length)) setDuration(length)
  }

  function activate() {
    if (!player.current) return
    updateMode('enabled')
    sync(true, true)
  }

  function retry() {
    setFailure('')
    setDuration(0)
    updateMode('loading')
    setAttempt(attempt + 1)
  }

  function play() {
    if (send({ type: 'play' }) && (mode === 'ready' || mode === 'blocked')) activate()
  }

  let note: string | undefined
  if (mode === 'blocked') {
    note = type === 'youtube'
      ? 'Браузер заблокировал запуск. Нажмите «Смотреть» или кнопку воспроизведения в самом плеере YouTube.'
      : 'Браузер заблокировал звук. Нажмите «Смотреть» ещё раз.'
  } else if (mode === 'ready') {
    note = canControl ? 'Воспроизведение и пауза в плеере действуют на всю комнату.'
      : 'Нажмите, чтобы разрешить воспроизведение со звуком.'
  } else if (mode === 'enabled') {
    note = !connected ? 'Синхронизация возобновится после подключения.'
      : canControl ? 'Воспроизведение, пауза и перемотка в плеере действуют на всю комнату.'
        : state.paused ? 'Воспроизведение в комнате на паузе.' : 'Воспроизведение синхронизировано с комнатой.'
  }

  return (
    <section className="player" aria-label="Видео комнаты">
      <div className="screen" aria-busy={mode === 'loading'}>
        {type === 'youtube' ? <div className="youtube-frame" ref={youtube} /> : <video ref={video} controls playsInline preload="metadata" aria-label="Видео комнаты" />}
        {mode === 'loading' && <div className="screen-message" role="status"><div className="skeleton" />Загружаем плеер…</div>}
        {mode === 'failed' && <div className="screen-message error-message" role="alert">{failure}</div>}
      </div>
      {mode !== 'loading' && <div className="player-note">
        {mode === 'failed' && <button type="button" className="button" onClick={retry}>Повторить загрузку</button>}
        {(mode === 'ready' || mode === 'blocked') && <button type="button" className="button primary" onClick={activate}><PlayIcon aria-hidden="true" />Смотреть</button>}
        <p aria-live="polite">{note}</p>
      </div>}
      {canControl && <RoomPlayback {...{ state, serverNow, duration, connected, play, send }} />}
    </section>
  )
}
