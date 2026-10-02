// Shared age client-side encryption helper. Encrypts a plaintext string to one or more age
// recipients IN THE BROWSER and returns the armored ciphertext -- the board/API only ever receives
// this, the plaintext never leaves the tab. Uses age-encryption (the official age TS implementation
// by FiloSottile; pure-JS @noble crypto), loaded lazily via dynamic import so the crypto bundle is
// fetched only when an encryption actually happens, not on every page. Used by the operator-
// question age-request answer (age-answer.tsx) -- the client-side encrypt for a secret-bearing
// question whose response_schema is a string holding the age ciphertext.
export async function encryptToRecipients(
  plaintext: string,
  recipients: string[],
): Promise<string> {
  if (recipients.length === 0) throw new Error('no recipients to encrypt to')
  const age = await import('age-encryption')
  const encrypter = new age.Encrypter()
  for (const recipient of recipients) encrypter.addRecipient(recipient) // throws on a malformed recipient
  const ciphertext = await encrypter.encrypt(plaintext)
  return age.armor.encode(ciphertext)
}
