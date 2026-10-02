import { useState } from 'react'
import { encryptToRecipients } from './age'
import { AutoGrowTextarea } from './ui'

// Dedicated answer control for the age-request element (task_822). The operator types the secret in
// plaintext; it is encrypted IN THE BROWSER to the question's age recipient (props.recipient) and
// only the armored ciphertext is submitted as the answer (shape text) -- the board never sees the
// plaintext. Encryption goes through the shared age helper (encryptToRecipients, which lazily loads
// the pure-JS age-encryption library). A paste-ciphertext fallback (the question's free-text escape)
// remains for operators who encrypt out of band.

const BTN = 'rounded-md bg-sky-700 px-2.5 py-1 text-xs font-medium text-white disabled:opacity-40'

export function AgeAnswer({
  recipient,
  busy,
  onSubmit,
}: {
  recipient: string
  busy: boolean
  onSubmit: (shape: string, value: unknown) => void
}) {
  const [secret, setSecret] = useState('')
  const [working, setWorking] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  const encryptAndSubmit = async () => {
    const plaintext = secret
    if (!plaintext || working || busy) return
    setWorking(true)
    setErr(null)
    try {
      const armored = await encryptToRecipients(plaintext, [recipient])
      onSubmit('text', armored)
      setSecret('')
    } catch (e) {
      setErr(e instanceof Error ? e.message : 'Encryption failed')
    } finally {
      setWorking(false)
    }
  }

  return (
    <div className="mt-2 space-y-1.5 rounded-md border border-[var(--color-border)] bg-[var(--color-panel)] p-2.5">
      <p className="text-xs text-[var(--color-muted)]">
        Encrypted in your browser to <span className="font-mono break-all">{recipient}</span>; only
        the ciphertext is sent -- the board never sees the plaintext.
      </p>
      <div className="flex items-end gap-2">
        <AutoGrowTextarea
          value={secret}
          onChange={setSecret}
          onSubmit={encryptAndSubmit}
          placeholder="Secret value..."
          className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-panel-2)] px-2 py-1 font-mono text-sm outline-none focus:border-sky-500/50"
        />
        <button
          disabled={busy || working || !secret}
          className={BTN}
          onClick={encryptAndSubmit}
        >
          {working ? 'Encrypting...' : 'Encrypt and submit'}
        </button>
      </div>
      {err && <p className="text-xs text-rose-400">{err}</p>}
    </div>
  )
}
