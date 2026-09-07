export interface FullscreenState { active: boolean; controls: boolean }

/** Only an actual fullscreenchange hides the controls, never a pending request. */
export class FullscreenView {
  private state: FullscreenState = { active: false, controls: true };
  private readonly document: Document;
  constructor(private readonly host: HTMLElement, private readonly changed: (state: FullscreenState) => void,
    private readonly failed: (message: string) => void) {
    this.document = host.ownerDocument;
    this.document.addEventListener('fullscreenchange', this.sync);
    this.document.addEventListener('keydown', this.keyDown);
    this.sync();
  }
  private readonly sync = () => {
    const active = this.document.fullscreenElement === this.host;
    this.state = { active, controls: active === this.state.active ? this.state.controls : !active };
    this.changed(this.state);
  };
  private readonly keyDown = (event: KeyboardEvent) => {
    if (event.key === 'F11') { event.preventDefault(); void this.toggle(); }
    if (event.key === 'Escape' && this.state.active) { void this.document.exitFullscreen().catch(() => {}); }
  };
  async toggle() {
    try {
      if (this.document.fullscreenElement === this.host) await this.document.exitFullscreen();
      else if (this.host.requestFullscreen) await this.host.requestFullscreen();
      else throw new Error('Fullscreen is unavailable in this window.');
    } catch (error) {
      this.failed(error instanceof Error ? error.message : 'Could not change fullscreen mode.');
    }
  }
  toggleControls() {
    if (!this.state.active) return;
    this.state = { ...this.state, controls: !this.state.controls };
    this.changed(this.state);
  }
  dispose() {
    this.document.removeEventListener('fullscreenchange', this.sync);
    this.document.removeEventListener('keydown', this.keyDown);
  }
}
