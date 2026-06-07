// Onboarding Tour - Interactive first-run walkthrough
// Shows on first visit, can be re-triggered from command palette or help

const STORAGE_KEY = 'ruvsense-onboarding-done';

export class Onboarding {
  constructor(app) {
    this.app = app;
    this.overlay = null;
    this.currentStep = 0;
    this.steps = [];
    this.active = false;
  }

  init() {
    this.defineSteps();
    document.addEventListener('start-onboarding', () => this.start());

    // Auto-start on first visit
    if (!this.isDone()) {
      // Delay to let the app render first
      setTimeout(() => this.start(), 800);
    }
  }

  defineSteps() {
    this.steps = [
      {
        title: 'Welcome to RuvSense Console',
        text: 'Operate the master, ESP32-C6 nodes, mesh APs, modules, calibration, and logs from one live page.',
        target: null, // No highlight, centered
        position: 'center'
      },
      {
        title: 'Fleet Status',
        text: 'The console shows readiness, active nodes, fusion status, and source mode without switching to synthetic data.',
        target: '#fleetState',
        position: 'bottom'
      },
      {
        title: 'Environment',
        text: 'Use the environment view to inspect your 2 AP mesh anchors, 3 ESP32-C6 sensing nodes, and live RF links.',
        target: '#environmentStage',
        position: 'bottom'
      },
      {
        title: 'Advanced Views',
        text: 'Buttons on the console open Observatory and Pose Fusion when you need deeper visualization.',
        target: '.top-actions',
        position: 'bottom'
      },
      {
        title: 'Keyboard Shortcuts',
        text: 'Press ? for shortcuts, Ctrl+K for the command palette, or use number keys 1-3 to open core pages.',
        target: null,
        position: 'center'
      },
      {
        title: 'You\'re all set!',
        text: 'Connect the Pi master and three ESP32-C6 nodes, then watch the console move from offline to live.',
        target: null,
        position: 'center'
      }
    ];
  }

  isDone() {
    try { return localStorage.getItem(STORAGE_KEY) === 'true'; }
    catch { return false; }
  }

  markDone() {
    try { localStorage.setItem(STORAGE_KEY, 'true'); }
    catch { /* noop */ }
  }

  start() {
    this.currentStep = 0;
    this.active = true;
    this.createOverlay();
    this.showStep();
  }

  createOverlay() {
    // Remove existing if any
    this.removeOverlay();

    this.overlay = document.createElement('div');
    this.overlay.className = 'onboarding-overlay';
    this.overlay.setAttribute('role', 'dialog');
    this.overlay.setAttribute('aria-label', 'Onboarding tour');
    this.overlay.setAttribute('aria-modal', 'true');
    document.body.appendChild(this.overlay);
  }

  showStep() {
    if (this.currentStep >= this.steps.length) {
      this.finish();
      return;
    }

    const step = this.steps[this.currentStep];
    const total = this.steps.length;
    const isFirst = this.currentStep === 0;
    const isLast = this.currentStep === total - 1;

    // Clear highlight
    document.querySelectorAll('.onboarding-highlight').forEach(el => el.classList.remove('onboarding-highlight'));

    // Highlight target
    let targetRect = null;
    if (step.target) {
      const targetEl = document.querySelector(step.target);
      if (targetEl) {
        targetEl.classList.add('onboarding-highlight');
        targetRect = targetEl.getBoundingClientRect();
      }
    }

    this.overlay.innerHTML = `
      <div class="onboarding-backdrop"></div>
      <div class="onboarding-tooltip ${step.position}" ${targetRect ? `style="${this.positionTooltip(targetRect, step.position)}"` : ''}>
        <div class="onboarding-progress">
          ${Array.from({ length: total }, (_, i) =>
            `<span class="onboarding-dot ${i === this.currentStep ? 'active' : i < this.currentStep ? 'done' : ''}"></span>`
          ).join('')}
        </div>
        <h3 class="onboarding-title">${step.title}</h3>
        <p class="onboarding-text">${step.text}</p>
        <div class="onboarding-actions">
          <button class="onboarding-skip">Skip tour</button>
          <div class="onboarding-nav">
            ${!isFirst ? '<button class="onboarding-prev">Back</button>' : ''}
            <button class="onboarding-next">${isLast ? 'Get started' : 'Next'}</button>
          </div>
        </div>
      </div>
    `;

    // Bind events
    this.overlay.querySelector('.onboarding-skip').addEventListener('click', () => this.finish());
    this.overlay.querySelector('.onboarding-next').addEventListener('click', () => {
      this.currentStep++;
      this.showStep();
    });
    const prevBtn = this.overlay.querySelector('.onboarding-prev');
    if (prevBtn) {
      prevBtn.addEventListener('click', () => {
        this.currentStep--;
        this.showStep();
      });
    }
    this.overlay.querySelector('.onboarding-backdrop').addEventListener('click', () => this.finish());

    // Focus next button
    this.overlay.querySelector('.onboarding-next').focus();

    // Escape to close
    this._escHandler = (e) => { if (e.key === 'Escape') this.finish(); };
    document.addEventListener('keydown', this._escHandler);
  }

  positionTooltip(rect, position) {
    const margin = 12;
    if (position === 'bottom') {
      return `left: ${Math.max(16, rect.left + rect.width / 2 - 180)}px; top: ${rect.bottom + margin}px;`;
    }
    if (position === 'top') {
      return `left: ${Math.max(16, rect.left + rect.width / 2 - 180)}px; bottom: ${window.innerHeight - rect.top + margin}px;`;
    }
    return '';
  }

  finish() {
    this.active = false;
    this.markDone();
    this.removeOverlay();
    document.querySelectorAll('.onboarding-highlight').forEach(el => el.classList.remove('onboarding-highlight'));
    if (this._escHandler) document.removeEventListener('keydown', this._escHandler);
  }

  removeOverlay() {
    if (this.overlay?.parentNode) {
      this.overlay.parentNode.removeChild(this.overlay);
      this.overlay = null;
    }
  }

  dispose() {
    this.finish();
  }
}
