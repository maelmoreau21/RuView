// Hardware Tab Component

export class HardwareTab {
  constructor(containerElement) {
    this.container = containerElement;
    this.antennas = [];
    this.csiUpdateInterval = null;
    this.isActive = false;
  }

  init() {
    this.setupAntennas();
    this.updateCSIDisplay();
  }

  setupAntennas() {
    this.antennas = Array.from(this.container.querySelectorAll('.antenna'));

    this.antennas.forEach(antenna => {
      antenna.addEventListener('click', () => {
        antenna.classList.toggle('active');
        this.updateCSIDisplay();
      });
    });
  }

  hasActiveAntennas() {
    return this.antennas.some(antenna => antenna.classList.contains('active'));
  }

  updateCSIDisplay() {
    const activeAntennas = this.antennas.filter(a => a.classList.contains('active'));
    const amplitudeFill = this.container.querySelector('.csi-fill.amplitude');
    const phaseFill = this.container.querySelector('.csi-fill.phase');
    const amplitudeValue = this.container.querySelector('.csi-row:first-child .csi-value');
    const phaseValue = this.container.querySelector('.csi-row:last-child .csi-value');

    if (amplitudeFill) {
      amplitudeFill.style.width = '0%';
      amplitudeFill.style.transition = 'width 0.5s ease';
    }
    if (phaseFill) {
      phaseFill.style.width = '0%';
      phaseFill.style.transition = 'width 0.5s ease';
    }
    if (amplitudeValue) amplitudeValue.textContent = '0.00';
    if (phaseValue) phaseValue.textContent = '0.0π';

    this.updateAntennaArray(activeAntennas);
  }

  updateAntennaArray(activeAntennas) {
    const arrayStatus = this.container.querySelector('.array-status');
    if (!arrayStatus) return;

    const txActive = activeAntennas.filter(a => a.classList.contains('tx')).length;
    const rxActive = activeAntennas.filter(a => a.classList.contains('rx')).length;

    arrayStatus.innerHTML = '';

    const createInfoDiv = (label, value) => {
      const div = document.createElement('div');
      div.className = 'array-info';

      const labelSpan = document.createElement('span');
      labelSpan.className = 'info-label';
      labelSpan.textContent = label;

      const valueSpan = document.createElement('span');
      valueSpan.className = 'info-value';
      valueSpan.textContent = value;

      div.appendChild(labelSpan);
      div.appendChild(valueSpan);
      return div;
    };

    arrayStatus.appendChild(createInfoDiv('Active TX:', `${txActive}/3`));
    arrayStatus.appendChild(createInfoDiv('Active RX:', `${rxActive}/6`));
    arrayStatus.appendChild(createInfoDiv('Signal Quality:', '0%'));
  }

  calculateSignalQuality() {
    return 0;
  }

  toggleAllAntennas(active) {
    this.antennas.forEach(antenna => {
      antenna.classList.toggle('active', active);
    });
    this.updateCSIDisplay();
  }

  resetAntennas() {
    this.antennas.forEach(antenna => {
      antenna.classList.remove('active');
    });
    this.updateCSIDisplay();
  }

  dispose() {
    if (this.csiUpdateInterval) {
      clearInterval(this.csiUpdateInterval);
      this.csiUpdateInterval = null;
    }

    this.antennas.forEach(antenna => {
      antenna.removeEventListener('click', this.toggleAntenna);
    });
  }
}
