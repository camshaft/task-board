import { useEffect, useState } from 'react'
import { useParams, useSearchParams } from 'react-router-dom'
import { api, type SecretRequest } from './api'

// Standalone secret-submission page (no board chrome), reached via the single-use capability
// link an agent hands the operator: /secret-requests/:id?t=<submit_token>. It fetches the
// request's non-secret metadata (name, instructions, recipients), then — entirely in the
// browser — encrypts the pasted value to the request's age recipients and posts ONLY the
// ciphertext. The board never receives plaintext: the no-store property holds by construction,
// not by policy. The age library is dynamically imported so its WASM lands only on this page.
export default function SecretSubmit() {
  const { id: idParam } = useParams()
  const [params] = useSearchParams()
  const token = params.get('t')
  const id = Number(idParam)

  const [req, setReq] = useState<SecretRequest | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [value, setValue] = useState('')
  const [busy, setBusy] = useState(false)
  const [submitError, setSubmitError] = useState<string | null>(null)
  const [done, setDone] = useState(false)

  useEffect(() => {
    let live = true
    api
      .getSecretRequest(id)
      .then((r) => live && setReq(r))
      .catch((e) => live && setLoadError(e instanceof Error ? e.message : String(e)))
    return () => {
      live = false
    }
  }, [id])

  async function submit() {
    if (!req || !token || busy) return
    setBusy(true)
    setSubmitError(null)
    try {
      if (req.recipients.length === 0) {
        throw new Error('this request has no recipients to encrypt to')
      }
      // Encrypt in the browser to the request's age recipients, then armor to text. The board
      // receives only this ciphertext — the pasted value never leaves the tab in the clear.
      const age = await import('age-encryption')
      const encrypter = new age.Encrypter()
      for (const recipient of req.recipients) encrypter.addRecipient(recipient)
      const ciphertext = await encrypter.encrypt(value)
      const armored = age.armor.encode(ciphertext)
      await api.submitSecret(id, { token, ciphertext: armored })
      setDone(true)
    } catch (e) {
      setSubmitError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="mx-auto flex min-h-full max-w-xl flex-col justify-center px-4 py-10">
      <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-panel)] p-6">
        <h1 className="text-lg font-semibold tracking-tight">
          <span className="text-sky-400">task</span>-board · submit a secret
        </h1>

        {loadError ? (
          <p className="mt-4 rounded-md border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-sm text-red-300">
            {loadError}
          </p>
        ) : !req ? (
          <p className="mt-4 text-sm text-[var(--color-muted)]">Loading request…</p>
        ) : done ? (
          <div className="mt-4 space-y-2">
            <p className="rounded-md border border-emerald-500/40 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-300">
              Submitted. The value was encrypted in your browser — the board received only
              ciphertext, never the plaintext.
            </p>
            <p className="text-sm text-[var(--color-muted)]">
              The fulfiller has been notified and will relocate <code>{req.name}</code> to its
              durable home. You can close this tab.
            </p>
          </div>
        ) : req.status !== 'requested' ? (
          <p className="mt-4 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm text-amber-300">
            This request is <strong>{req.status}</strong> — it isn&apos;t awaiting a submission.
            A submit link is single-use; if you need to resubmit, ask for a fresh request.
          </p>
        ) : !token ? (
          <p className="mt-4 rounded-md border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-sm text-red-300">
            This link is missing its access token (<code>?t=…</code>). Use the full submit link
            you were given.
          </p>
        ) : (
          <div className="mt-4 space-y-4">
            <div>
              <div className="text-sm text-[var(--color-muted)]">Secret name</div>
              <div className="font-mono text-sm">{req.name}</div>
            </div>
            {req.instructions ? (
              <div>
                <div className="text-sm text-[var(--color-muted)]">Instructions</div>
                <p className="whitespace-pre-wrap text-sm">{req.instructions}</p>
              </div>
            ) : null}
            <div>
              <label htmlFor="secret-value" className="text-sm text-[var(--color-muted)]">
                Value
              </label>
              <textarea
                id="secret-value"
                value={value}
                onChange={(e) => setValue(e.target.value)}
                disabled={busy}
                rows={4}
                autoComplete="off"
                spellCheck={false}
                placeholder="Paste the secret value here"
                className="mt-1 w-full rounded-md border border-[var(--color-border)] bg-[var(--color-bg)] px-3 py-2 font-mono text-sm outline-none focus:border-sky-500 disabled:opacity-60"
              />
            </div>
            <p className="text-xs text-[var(--color-muted)]">
              Encrypted in your browser to {req.recipients.length} recipient
              {req.recipients.length === 1 ? '' : 's'} before it&apos;s sent — the board only ever
              receives ciphertext. Submitted exactly as entered.
            </p>
            {submitError ? (
              <p className="rounded-md border border-rose-500/40 bg-rose-500/10 px-3 py-2 text-sm text-red-300">
                {submitError}
              </p>
            ) : null}
            <button
              type="button"
              onClick={submit}
              disabled={busy || value.length === 0}
              className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? 'Encrypting + submitting…' : 'Encrypt in browser + submit'}
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
