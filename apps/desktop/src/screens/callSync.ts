export class SnapshotSync {
  private revision = 0;

  begin(): number {
    this.revision += 1;
    return this.revision;
  }

  invalidate(): void {
    this.revision += 1;
  }

  isLatest(revision: number): boolean {
    return revision === this.revision;
  }
}
