export type PrivateKind = 'file' | 'directory'

/** Normalized owner/DACL fields for testing the owner-only policy. Runtime checks
 * must query the opened object's OS descriptor; this input alone cannot prove that access is owner-only. */
export interface PrivateDescriptor {
  owner: string
  currentUser: string
  defaultOwner: string
  kind: PrivateKind
  parent?: PrivateDescriptor
  reparse: boolean
  daclPresent: boolean
  daclProtected: boolean
  aces: null | { type: number; flags: number; mask: number; sid: string }[]
}

export function isPrivateDescriptor(
  descriptor: PrivateDescriptor,
  kind: PrivateKind,
): boolean {
  const ace = descriptor.aces?.[0]
  return (
    (descriptor.owner === descriptor.currentUser ||
      descriptor.owner === 'S-1-5-32-544' ||
      descriptor.owner === 'S-1-5-18') &&
    descriptor.kind === kind &&
    !descriptor.reparse &&
    descriptor.daclPresent &&
    ((descriptor.daclProtected &&
      ace?.flags === (kind === 'directory' ? 3 : 0)) ||
      (kind === 'file' &&
        !descriptor.daclProtected &&
        ace?.flags === 16 &&
        !!descriptor.parent &&
        descriptor.parent.currentUser === descriptor.currentUser &&
        isPrivateDescriptor(descriptor.parent, 'directory'))) &&
    descriptor.aces?.length === 1 &&
    ace?.type === 0 &&
    ace.mask === 0x1f01ff &&
    ace.sid === descriptor.currentUser
  )
}
